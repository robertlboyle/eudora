//! Deciding where a message goes, and why (triage step 2; the project's design notes).
//!
//! Rspamd-style: every check that fires is a named `Symbol` with a weight and a plain-English
//! reason; the decision follows fixed rules over them (explicit lists first, then "like this"
//! rules, spam, priority, people, lists, bulk). The symbols are the "why" the user sees.
//!
//! The dry run (`Library::triage_preview`) decides for a folder's messages without moving
//! anything; `Library::triage_apply` moves (locally, and on the server through the pending
//! changes) only what the user kept ticked.

use std::collections::HashMap;

use rayon::prelude::*;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::library::now;
use crate::triage::{Action, SenderLists, Signals, signals};
use crate::Library;
use crate::rules::{Facts, Test};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// From a blacklisted sender: its entry's action.
    Blocked,
    /// Matched a "messages like this one" rule: its action.
    Rule,
    Spam,
    /// Flagged high priority (stays in the Inbox).
    Priority,
    /// From someone the user corresponds with (whitelist; stays).
    People,
    /// A mailing list.
    List,
    /// Marketing: mass mailings (newsletters, offers), also from automatically whitelisted
    /// senders. Shown to the user as "Marketing"; "bulk" in data for continuity.
    Bulk,
    /// Machine-sent mail about the user (orders, alerts, receipts, signatures, tickets).
    Notification,
    /// Nothing says otherwise: stays.
    Other,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::Blocked => "blocked",
            Category::Rule => "rule",
            Category::Spam => "spam",
            Category::Priority => "priority",
            Category::People => "people",
            Category::List => "list",
            Category::Bulk => "bulk",
            Category::Notification => "notification",
            Category::Other => "other",
        }
    }
}

/// One check that fired.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Symbol {
    pub name: &'static str,
    /// The rule it belongs to (`FilterRule::key`): what to switch off, or give an exception.
    pub rule: String,
    /// Spam weight (+ = spammy, − = trustworthy); 0 for facts that only sort.
    pub score: f32,
    pub why: String,
}

/// Where a message should go, and why.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub category: Category,
    /// Flag it high priority.
    pub priority: bool,
    /// What to do: None = leave it where it is.
    pub action: Option<Action>,
    pub spam_score: f32,
    pub symbols: Vec<Symbol>,
    /// Bulk mail: who sent it (`bulk_sender`) and their name, for a mailbox of its own under Bulk.
    pub sender_key: Option<String>,
    pub sender_name: Option<String>,
}

/// A spam score from here up to the spam line (5) is "maybe": the mail stays in the Inbox and is
/// listed in the Maybe Spam view.
pub const MAYBE_SCORE: f32 = 2.5;

/// The view (tag) undecided mail is listed in.
pub const MAYBE_VIEW: &str = "maybe-spam";

impl Decision {
    /// Mail that stays where it is though it looks a little like spam (never mail from people
    /// on the whitelist).
    pub fn is_maybe(&self) -> bool {
        self.action.is_none() && matches!(self.category, Category::Other | Category::Priority) && self.spam_score >= MAYBE_SCORE && self.spam_score < 5.0
    }
}

// ------------------------------------------------------------------ "messages like this one"

/// Shared domains of bulk-mail services and free mail: never enough on their own to say "like
/// this" (they'd catch thousands of unrelated senders).
const SHARED_DOMAINS: [&str; 24] = [
    "sendgrid.net", "mcsv.net", "mcdlv.net", "rsgsv.net", "mandrillapp.com", "amazonses.com", "mailgun.org", "hubspotemail.net", "exacttarget.com",
    "klaviyomail.com", "kmail-lists.com", "ccsend.com", "constantcontact.com", "sparkpostmail.com", "mktomail.com", "mtasv.net", "sendinblue.com", "brevo.com",
    "gmail.com", "outlook.com", "hotmail.com", "yahoo.com", "icloud.com", "onmicrosoft.com",
];

/// The registrable ("organizational") domain: mail.shop.co.uk → shop.co.uk.
pub fn org_domain(domain: &str) -> String {
    let d = domain.trim().trim_end_matches('.').to_lowercase();
    psl::domain_str(&d).map(str::to_string).unwrap_or(d)
}

pub(crate) fn shared(org: &str) -> bool {
    SHARED_DOMAINS.contains(&org)
}

/// A subject with what changes from one mailing to the next taken out (numbers, dates,
/// amounts): "Your order #12345 has shipped" → "your order # has shipped".
pub fn subject_template(subject: &str) -> String {
    let lower = subject.to_lowercase();
    let mut out = String::new();
    let mut in_number = false;
    for c in lower.chars() {
        if c.is_ascii_digit() || (in_number && matches!(c, ',' | '.' | '/' | ':' | '-')) {
            if !in_number && !out.ends_with('#') {
                out.push('#');
            }
            in_number = true;
        } else {
            in_number = false;
            out.push(c);
        }
    }
    out.split_whitespace().filter(|w| *w != "re:" && *w != "fw:" && *w != "fwd:").collect::<Vec<_>>().join(" ").replace("# #", "#")
}

/// The parts of a message that stay the same across a sender's mailings (strongest first).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fingerprint {
    pub list_id: Option<String>,
    /// Feedback-ID's sender id and stream (the last two parts), when DKIM passed.
    pub feedback: Option<String>,
    /// The DKIM signer's organizational domain, when DKIM passed and it isn't a shared domain.
    pub dkim_org: Option<String>,
    /// Where bounces go (Return-Path), organizational domain, unless shared.
    pub bounce_org: Option<String>,
    /// From's organizational domain, unless shared.
    pub from_org: Option<String>,
    /// From's address, for senders on shared domains.
    pub from_address: Option<String>,
    pub subject: Option<String>,
}

impl Fingerprint {
    pub fn of(s: &Signals, subject: &str) -> Fingerprint {
        let dkim_ok = s.dkim.as_deref() == Some("pass");
        let feedback = s.feedback_id.as_deref().filter(|_| dkim_ok).and_then(|f| {
            let parts: Vec<&str> = f.split(':').map(str::trim).filter(|p| !p.is_empty()).collect();
            (parts.len() >= 2).then(|| parts[parts.len() - 2..].join(":").to_lowercase())
        });
        let keep = |d: String| (!d.is_empty() && !shared(&d)).then_some(d);
        let from_org = keep(org_domain(&s.from_domain));
        Fingerprint {
            list_id: s.list_id.clone().map(|l| l.to_lowercase()),
            feedback,
            dkim_org: s.dkim_domains.first().filter(|_| dkim_ok).and_then(|d| keep(org_domain(d))),
            bounce_org: s.return_path_domain.as_deref().and_then(|d| keep(org_domain(d))),
            from_address: if from_org.is_none() && !s.from.is_empty() { Some(s.from.clone()) } else { None },
            from_org,
            subject: Some(subject_template(subject)).filter(|t| t.len() >= 6),
        }
    }

    /// The rule a "messages like this one" request makes: one strong key, or two weaker ones.
    pub fn rule_keys(&self) -> Fingerprint {
        let mut k = Fingerprint::default();
        if self.list_id.is_some() {
            k.list_id = self.list_id.clone();
        } else if self.feedback.is_some() {
            k.feedback = self.feedback.clone();
        } else if self.dkim_org.is_some() && self.dkim_org == self.from_org {
            k.dkim_org = self.dkim_org.clone();
        } else if let Some(from) = &self.from_org {
            // the sender's own domain, narrowed by where bounces go or what the subjects say
            k.from_org = Some(from.clone());
            if self.bounce_org.is_some() && self.bounce_org != self.from_org {
                k.bounce_org = self.bounce_org.clone();
            } else if self.dkim_org.is_some() {
                k.dkim_org = self.dkim_org.clone();
            }
        } else if let Some(address) = &self.from_address {
            // a sender on a shared domain (gmail.com, sendgrid.net): the address itself
            k.from_address = Some(address.clone());
        } else {
            k.subject = self.subject.clone();
        }
        k
    }

    /// Every key this rule has matches `other`.
    pub fn matches(&self, other: &Fingerprint) -> bool {
        let same = |a: &Option<String>, b: &Option<String>| a.is_none() || a == b;
        let any = [&self.list_id, &self.feedback, &self.dkim_org, &self.bounce_org, &self.from_org, &self.from_address, &self.subject].iter().any(|k| k.is_some());
        any && same(&self.list_id, &other.list_id)
            && same(&self.feedback, &other.feedback)
            && same(&self.dkim_org, &other.dkim_org)
            && same(&self.bounce_org, &other.bounce_org)
            && same(&self.from_org, &other.from_org)
            && same(&self.from_address, &other.from_address)
            && same(&self.subject, &other.subject)
    }

    /// For the user: what this rule looks at.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = &self.list_id {
            parts.push(format!("the mailing list {v}"));
        }
        if let Some(v) = &self.feedback {
            parts.push(format!("the sender's mailing id {v}"));
        }
        if let Some(v) = &self.dkim_org {
            parts.push(format!("signed by {v}"));
        }
        if let Some(v) = &self.from_org {
            parts.push(format!("from {v}"));
        }
        if let Some(v) = &self.from_address {
            parts.push(format!("from {v}"));
        }
        if let Some(v) = &self.bounce_org {
            parts.push(format!("bounces going to {v}"));
        }
        if let Some(v) = &self.subject {
            parts.push(format!("subjects like “{v}”"));
        }
        parts.join(", ")
    }
}

/// A saved "messages like this one" rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LikeRule {
    pub id: i64,
    pub name: String,
    pub keys: Fingerprint,
    pub action: Action,
    pub created: i64,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The mailbox it applies in (its account's address); empty = every mailbox.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mailbox: String,
}

fn yes() -> bool {
    true
}

// ------------------------------------------------------------------ priority

/// Phrase groups that flag a message high priority (the user can edit them: setting
/// "triage-phrases"). `upset` counts only from people the user corresponds with or non-bulk mail.
pub fn default_phrases() -> Vec<(String, Vec<String>)> {
    let group = |name: &str, list: &[&str]| (name.to_string(), list.iter().map(|s| s.to_string()).collect());
    vec![
        group("Service cut-off / money owed", &["disconnect notice", "disconnection notice", "service will be disconnected", "service will be suspended", "shut-off notice", "shutoff notice", "final notice", "past due", "overdue", "sent to collections", "referred to collections", "collection agency", "account is delinquent", "late payment"]),
        group("Taxes", &["taxes due", "tax due", "balance due", "notice of assessment", "irs notice", "estimated tax", "property tax", "tax lien", "franchise tax"]),
        group("Shipments", &["shipment exception", "delivery exception", "unable to deliver", "delivery attempt failed", "returned to sender", "held at customs", "customs clearance", "address correction required"]),
        group("Legal", &["subpoena", "lawsuit", "summons", "cease and desist", "legal action", "court date"]),
        group("Upset customers", &["very disappointed", "unacceptable", "cancel my service", "cancel my account", "cancel our service", "want a refund", "chargeback", "file a complaint", "escalate this", "speak to a manager", "extremely frustrated", "this is ridiculous", "my lawyer"]),
    ]
}

pub struct Phrases {
    groups: Vec<String>,
    /// Pattern index → its group.
    group_of: Vec<usize>,
    finder: aho_corasick::AhoCorasick,
}

impl Phrases {
    pub fn new(groups: &[(String, Vec<String>)]) -> Phrases {
        let mut patterns = Vec::new();
        let mut group_of = Vec::new();
        for (i, (_, list)) in groups.iter().enumerate() {
            for p in list {
                patterns.push(p.to_lowercase());
                group_of.push(i);
            }
        }
        let finder = aho_corasick::AhoCorasick::builder().ascii_case_insensitive(true).build(&patterns).expect("phrase list");
        Phrases { groups: groups.iter().map(|(g, _)| g.clone()).collect(), group_of, finder }
    }

    /// (group, the phrase found) for each group with a hit.
    pub fn find(&self, text: &str) -> Vec<(String, String)> {
        let mut seen = Vec::new();
        let mut out = Vec::new();
        let word_char = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric());
        for m in self.finder.find_overlapping_iter(text) {
            // whole words only ("overdue" yes, "collections" inside "recollections" no)
            if word_char(text[..m.start()].chars().next_back()) || word_char(text[m.end()..].chars().next()) {
                continue;
            }
            let g = self.group_of[m.pattern().as_usize()];
            if !seen.contains(&g) {
                seen.push(g);
                out.push((self.groups[g].clone(), text[m.start()..m.end()].to_string()));
            }
        }
        out
    }
}

// ------------------------------------------------------------------ deciding

/// What deciding needs besides the message: the lists, the rules, the phrases.
pub struct Context {
    /// The whitelist and blacklist entries for every mailbox.
    pub lists: SenderLists,
    /// Mailboxes with entries of their own (address → every-mailbox entries + its own).
    pub mailbox_lists: HashMap<String, SenderLists>,
    pub rules: Vec<LikeRule>,
    pub phrases: Phrases,
    /// Bulk senders with a mailbox of their own: sender key → folder ("Bulk/Travelzoo").
    pub bulk_folders: HashMap<String, String>,
    /// The user's primary languages (ISO 639-1 codes: "en", "it").
    pub languages: Vec<String>,
    /// The priority phrase groups (each is a rule of its own).
    pub phrases_groups: Vec<(String, Vec<String>)>,
    /// Rules switched off, and their exceptions and inclusions (by rule key).
    pub rule_settings: HashMap<String, RuleSettings>,
    /// The user's own organizations' domains (example.com): their mail is kept by address.
    pub own_domains: Vec<String>,
    /// Each mailbox's learning (by its address, lowercase), when it has learned enough.
    pub bayes: HashMap<String, std::sync::Arc<crate::bayes::Model>>,
    /// What's known of senders' organization domains (registration, first mail).
    pub domains: HashMap<String, crate::reputation::DomainFacts>,
    /// Blocklist listings known now: (list, name) → what it means.
    pub listings: HashMap<(String, String), String>,
}

/// Mail in other languages.
pub const OTHER_LANGUAGES_FOLDER: &str = SPAM_FOLDER;

/// Where Eudora puts spam: one folder of its own, apart from the server's Junk Email (what
/// Microsoft 365 and the security gateway put there).
pub const SPAM_FOLDER: &str = "Spam";

/// Languages the user can choose, with the writing system each uses.
pub const LANGUAGES: [(&str, &str, Script); 30] = [
    ("en", "English", Script::Latin), ("it", "Italian", Script::Latin), ("es", "Spanish", Script::Latin), ("fr", "French", Script::Latin),
    ("de", "German", Script::Latin), ("pt", "Portuguese", Script::Latin), ("nl", "Dutch", Script::Latin), ("pl", "Polish", Script::Latin),
    ("tr", "Turkish", Script::Latin), ("vi", "Vietnamese", Script::Latin), ("id", "Indonesian", Script::Latin), ("sv", "Swedish", Script::Latin),
    ("ro", "Romanian", Script::Latin), ("cs", "Czech", Script::Latin), ("tl", "Filipino", Script::Latin), ("ar", "Arabic", Script::Arabic),
    ("fa", "Persian", Script::Arabic), ("ur", "Urdu", Script::Arabic), ("ru", "Russian", Script::Cyrillic), ("uk", "Ukrainian", Script::Cyrillic),
    ("el", "Greek", Script::Greek), ("he", "Hebrew", Script::Hebrew), ("zh", "Chinese", Script::Han), ("ja", "Japanese", Script::Japanese),
    ("ko", "Korean", Script::Hangul), ("hi", "Hindi", Script::Devanagari), ("th", "Thai", Script::Thai), ("bn", "Bengali", Script::Bengali),
    ("ta", "Tamil", Script::Tamil), ("hy", "Armenian", Script::Armenian),
];

/// Writing systems, told apart by their Unicode ranges (no library needed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum Script { Latin, Arabic, Cyrillic, Greek, Hebrew, Han, Japanese, Hangul, Devanagari, Thai, Bengali, Tamil, Armenian }

impl Script {
    fn of(c: char) -> Option<Script> {
        Some(match c as u32 {
            0x41..=0x5A | 0x61..=0x7A | 0xC0..=0x24F | 0x1E00..=0x1EFF => Script::Latin,
            0x600..=0x6FF | 0x750..=0x77F | 0x8A0..=0x8FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => Script::Arabic,
            0x400..=0x52F => Script::Cyrillic,
            0x370..=0x3FF | 0x1F00..=0x1FFF => Script::Greek,
            0x590..=0x5FF => Script::Hebrew,
            0x3040..=0x30FF => Script::Japanese,
            0x4E00..=0x9FFF | 0x3400..=0x4DBF => Script::Han,
            0xAC00..=0xD7AF | 0x1100..=0x11FF => Script::Hangul,
            0x900..=0x97F => Script::Devanagari,
            0xE00..=0xE7F => Script::Thai,
            0x980..=0x9FF => Script::Bengali,
            0xB80..=0xBFF => Script::Tamil,
            0x530..=0x58F => Script::Armenian,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Script::Latin => "a Latin-alphabet language",
            Script::Arabic => "Arabic script",
            Script::Cyrillic => "Cyrillic (Russian, Ukrainian…)",
            Script::Greek => "Greek",
            Script::Hebrew => "Hebrew",
            Script::Han => "Chinese",
            Script::Japanese => "Japanese",
            Script::Hangul => "Korean",
            Script::Devanagari => "Devanagari (Hindi…)",
            Script::Thai => "Thai",
            Script::Bengali => "Bengali",
            Script::Tamil => "Tamil",
            Script::Armenian => "Armenian",
        }
    }
}

pub fn language_names(codes: &[String]) -> String {
    codes.iter().map(|c| LANGUAGES.iter().find(|l| l.0 == c).map(|l| l.1).unwrap_or(c.as_str())).collect::<Vec<_>>().join(", ")
}

/// The writing system a text is mostly in, when none of the user's languages use it: at least
/// 20 letters, more than half of them in that system. Japanese mixes in Chinese characters, so
/// either counts for Japanese readers. (Telling Spanish from Italian needs word statistics: not
/// done here.)
/// The body without what mailing lists and signatures add at the end: everything after a "--"
/// line, and lines about unsubscribing (a list's English footer under an Arabic message).
pub fn body_without_footer(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines().take(400) {
        let t = line.trim();
        if t == "--" || t == "-- " || t == "__" {
            break;
        }
        if t.to_ascii_lowercase().contains("unsubscribe") || t.to_ascii_lowercase().contains("you received this message because") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// A subject written mostly in a script the user doesn't read (at least 8 letters of it).
pub fn foreign_subject(subject: &str, languages: &[String]) -> Option<&'static str> {
    foreign_in(subject, languages, 8)
}

pub fn foreign_writing(text: &str, languages: &[String]) -> Option<&'static str> {
    foreign_in(text, languages, 20)
}

/// The script most of `text`'s letters are in, when it's one the user doesn't read (`least`:
/// letters needed to tell).
fn foreign_in(text: &str, languages: &[String], least: usize) -> Option<&'static str> {
    if languages.is_empty() {
        return None;
    }
    let mut counts: HashMap<Script, usize> = HashMap::new();
    let mut letters = 0;
    for c in text.chars() {
        if let Some(script) = Script::of(c) {
            *counts.entry(script).or_default() += 1;
            letters += 1;
        }
    }
    let (main, n) = counts.into_iter().max_by_key(|(_, n)| *n)?;
    if letters < least || n * 2 <= letters {
        return None;
    }
    let readable = |s: Script| languages.iter().any(|code| LANGUAGES.iter().any(|(c, _, ls)| c == code && (*ls == s || (*ls == Script::Japanese && s == Script::Han))));
    if !readable(main) {
        return Some(main.name());
    }
    // the same alphabet as one of the user's (French vs English): told by its little words
    if main == Script::Latin { latin_foreign(text, languages, least < 20) } else { None }
}

/// The writing system a character set is made for (the non-Latin ones only: Latin sets say
/// nothing useful).
fn charset_script(charset: &str) -> Option<Script> {
    let c = charset.replace('_', "-");
    Some(match c.as_str() {
        "gb2312" | "gbk" | "gb18030" | "big5" | "big5-hkscs" | "hz-gb-2312" | "euc-cn" | "euc-tw" => Script::Han,
        "iso-2022-jp" | "shift-jis" | "sjis" | "euc-jp" | "windows-31j" | "cp932" => Script::Japanese,
        "euc-kr" | "ks-c-5601-1987" | "iso-2022-kr" | "cp949" => Script::Hangul,
        "koi8-r" | "koi8-u" | "windows-1251" | "cp1251" | "iso-8859-5" => Script::Cyrillic,
        "windows-1256" | "cp1256" | "iso-8859-6" => Script::Arabic,
        "windows-1253" | "iso-8859-7" => Script::Greek,
        "windows-1255" | "iso-8859-8" | "iso-8859-8-i" => Script::Hebrew,
        "tis-620" | "windows-874" | "iso-8859-11" => Script::Thai,
        _ => return None,
    })
}

/// A character set declared for a writing system none of the user's languages use: its
/// name. Only a hint (+1): what decides is the text itself.
pub fn foreign_charset<'a>(charsets: &'a [String], languages: &[String]) -> Option<(&'a str, &'static str)> {
    if languages.is_empty() {
        return None;
    }
    charsets.iter().find_map(|c| {
        let script = charset_script(c)?;
        let readable = languages.iter().any(|code| LANGUAGES.iter().any(|(l, _, s)| l == code && (*s == script || (*s == Script::Japanese && script == Script::Han))));
        (!readable).then_some((c.as_str(), script.name()))
    })
}

/// The most common little words of the Latin-alphabet languages most mail comes in. No
/// library: these tell the big ones apart well in a few lines of text.
const LITTLE_WORDS: [(&str, &[&str]); 7] = [
    ("en", &["the", "and", "of", "to", "is", "are", "you", "your", "for", "with", "this", "that", "have", "will", "be", "on", "we", "our", "from", "not", "at", "by", "or", "can", "please", "thank", "thanks", "would", "if", "was", "were", "has", "been", "about", "it", "an"]),
    ("it", &["il", "lo", "gli", "di", "del", "della", "dei", "delle", "degli", "che", "non", "per", "con", "una", "sono", "è", "questo", "questa", "anche", "come", "più", "ma", "nel", "nella", "alla", "ti", "ci", "grazie", "buongiorno", "ciao", "siamo", "hai", "abbiamo", "essere", "gentile", "cordiali", "saluti"]),
    ("fr", &["le", "les", "des", "du", "et", "est", "une", "pour", "dans", "que", "qui", "vous", "nous", "pas", "sur", "avec", "au", "aux", "ce", "cette", "sont", "par", "plus", "être", "votre", "vos", "notre", "nos", "mais", "où", "très", "merci", "bonjour", "qu", "été", "leur"]),
    ("es", &["el", "los", "las", "del", "y", "es", "una", "para", "con", "que", "por", "en", "su", "sus", "se", "al", "como", "más", "pero", "este", "esta", "usted", "nuestro", "gracias", "hola", "son", "está", "muy"]),
    ("de", &["der", "die", "das", "und", "ist", "nicht", "mit", "ein", "eine", "für", "sie", "wir", "ich", "auf", "den", "dem", "zu", "von", "im", "auch", "sich", "bitte", "danke", "ihre", "ihr", "wird", "werden", "sind", "haben", "oder"]),
    ("pt", &["os", "um", "uma", "do", "da", "dos", "das", "é", "para", "com", "não", "que", "em", "no", "na", "por", "mais", "seu", "sua", "você", "obrigado", "obrigada", "são", "está", "muito", "nós"]),
    ("nl", &["de", "het", "een", "en", "is", "van", "niet", "met", "voor", "op", "dat", "die", "zijn", "ook", "je", "wij", "uw", "naar", "maar", "bij", "worden", "wordt", "hebben", "dank", "graag"]),
];

/// A Latin-alphabet text in one of those languages that isn't the user's: its name. Only when
/// it's clear: enough of its little words, and well ahead of the user's own languages (a
/// bilingual message, or English with a French name in it, stays).
fn latin_foreign(text: &str, languages: &[String], short: bool) -> Option<&'static str> {
    let mut hits = [0usize; LITTLE_WORDS.len()];
    let mut words = 0usize;
    for word in text.split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()).take(3000) {
        words += 1;
        let word = word.to_lowercase();
        for (i, (_, list)) in LITTLE_WORDS.iter().enumerate() {
            if list.contains(&word.as_str()) {
                hits[i] += 1;
            }
        }
    }
    let mine = |code: &str| languages.iter().any(|l| l == code);
    let (best, &n) = hits.iter().enumerate().filter(|(i, _)| !mine(LITTLE_WORDS[*i].0)).max_by_key(|(_, n)| **n)?;
    let own = hits.iter().enumerate().filter(|(i, _)| mine(LITTLE_WORDS[*i].0)).map(|(_, n)| *n).max();
    // a language the table doesn't know among the user's: never judged
    if languages.iter().any(|l| LANGUAGES.iter().any(|(c, _, s)| c == l && *s == Script::Latin) && !LITTLE_WORDS.iter().any(|(c, _)| c == l)) {
        return None;
    }
    let own = own.unwrap_or(0);
    let clear = if short { n >= 3 && own == 0 } else { n >= 6 && n * 100 >= words * 8 && n * 2 >= own * 3 + 2 };
    clear.then(|| LANGUAGES.iter().find(|(c, _, _)| *c == LITTLE_WORDS[best].0).map(|l| l.1)).flatten()
}

/// Bulk mail comes in two kinds: marketing (the same mail to everyone: newsletters, offers)
/// and notifications (machine-sent but about you: orders, alerts, receipts, signatures,
/// tickets).
pub const BULK_FOLDER: &str = "Bulk";
pub const MARKETING_FOLDER: &str = "Bulk/Marketing";
pub const NOTIFICATIONS_FOLDER: &str = "Notifications";
/// A marketing sender gets a mailbox of its own once it has sent more than this many…
pub const BULK_FOLDER_AT: usize = 100;
/// …a notification sender from this many (Amazon, DocuSign, Jira…; rare ones share the folder).
pub const NOTIFICATION_FOLDER_AT: usize = 10;

/// Marketing services (as opposed to the ones mostly used for transactional mail: Amazon SES,
/// SendGrid, Mailgun, Postmark, SparkPost, Mandrill).
const MARKETING_SERVICES: [&str; 7] = ["Mailchimp", "Klaviyo", "Constant Contact", "Marketo", "HubSpot", "Salesforce Marketing Cloud", "a mailing service"];

/// Whether bulk mail is marketing rather than a notification: it carries what mass mailings
/// must (an unsubscribe link, Precedence: bulk), Microsoft rates it bulk, or a marketing service
/// sent it. Notifications are the machine-sent mail without those.
pub fn is_marketing(s: &Signals) -> bool {
    s.unsubscribe
        || matches!(s.precedence.as_deref(), Some("bulk") | Some("junk"))
        || s.bcl.is_some_and(|b| b >= 4)
        || s.esp.as_deref().is_some_and(|e| MARKETING_SERVICES.iter().any(|m| e.starts_with(m)))
}

/// Who a bulk message is from, across the addresses a company rotates through: the From
/// address's organizational domain (travelzoo.com), or the address itself on a shared domain.
pub fn bulk_sender(s: &Signals) -> String {
    bulk_sender_for(s, &[])
}

/// As `bulk_sender`, but mail from the user's own organization (`own`: their accounts'
/// organizational domains) is kept by address: each of the company's systems gets its own
/// folder (voicemail, backups, reports), not one for the whole company.
pub fn bulk_sender_for(s: &Signals, own: &[String]) -> String {
    if own.iter().any(|d| *d == org_domain(&s.from_domain)) {
        return s.from.clone();
    }
    bulk_sender_plain(s)
}

fn bulk_sender_plain(s: &Signals) -> String {
    let org = org_domain(&s.from_domain);
    // mail claiming a well-known sender that doesn't prove it is kept apart from the real one's
    // folder (by its own address)
    let unproven = crate::triage_known::known(&s.from).is_some() && !crate::triage_known::verified(s);
    if org.is_empty() || shared(&org) || unproven { s.from.clone() } else { org }
}

/// The two kinds, as `Decision::sender_key` carries them: "marketing:<sender>",
/// "notifications:<sender>". Folders remembered before the split have the bare sender.
fn sender_key(kind: Category, sender: &str) -> String {
    format!("{}:{sender}", if kind == Category::Notification { "notifications" } else { "marketing" })
}

/// The mailbox a bulk message goes to: its sender's own, else the kind's.
fn bulk_destination(kind: Category, sender: &str, folders: &HashMap<String, String>) -> String {
    let own = folders.get(&sender_key(kind, sender)).or_else(|| if kind == Category::Bulk { folders.get(sender) } else { None });
    own.cloned().unwrap_or_else(|| if kind == Category::Notification { NOTIFICATIONS_FOLDER } else { MARKETING_FOLDER }.into())
}

/// A mailbox name: the sender's usual display name when most of its mail uses one, else the
/// company from its domain ("Jane Smith via Docusign", "Bob via Docusign" → "Docusign").
pub fn bulk_folder_name(parent: &str, names: &[&str], sender: &str) -> String {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for n in names {
        let clean: String = n.chars().map(|c| if matches!(c, '/' | '\\' | '"' | ':' | '*' | '?' | '<' | '>' | '|') { ' ' } else { c }).collect();
        let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
        if !clean.is_empty() && !clean.contains('@') {
            *counts.entry(clean).or_default() += 1;
        }
    }
    let usual = counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0))).filter(|(_, n)| *n * 2 >= names.len().max(1));
    let name = match (crate::triage_known::folder_name(sender), usual) {
        (Some(known), _) => known.to_string(),
        (None, Some((n, _))) => n,
        (None, None) => company_name(sender),
    };
    let name: String = name.chars().take(60).collect();
    format!("{parent}/{}", name.trim())
}

/// "docusign.net" → "Docusign"; "acmefiber.atlassian.net" → "Acmefiber"; an
/// address keeps its part before the @.
fn company_name(sender: &str) -> String {
    let base = sender.split('@').next().filter(|_| sender.contains('@')).unwrap_or_else(|| sender.split('.').next().unwrap_or(sender));
    // short names are usually initials: "td" → "TD", "usps" → "USPS"
    if base.len() <= 5 && base.chars().all(|c| c.is_ascii_alphabetic()) {
        return base.to_uppercase();
    }
    let mut chars = base.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => sender.to_string(),
    }
}

/// Senders with enough messages in the run (and no mailbox yet) get one: marketing over
/// `BULK_FOLDER_AT`, notifications from `NOTIFICATION_FOLDER_AT`; their proposals move there.
/// Nothing is made until applied.
pub fn give_bulk_senders_folders(proposals: &mut [Proposal], existing: &HashMap<String, String>) {
    let mut by_sender: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in proposals.iter().enumerate() {
        if let Some(key) = &p.decision.sender_key {
            let bare = key.split_once(':').map(|(_, s)| s).unwrap_or(key);
            if !existing.contains_key(key) && !(key.starts_with("marketing:") && existing.contains_key(bare)) {
                by_sender.entry(key.clone()).or_default().push(i);
            }
        }
    }
    let mut taken: Vec<String> = existing.values().map(|f| f.to_lowercase()).collect();
    let enough = |key: &str, n: usize| if key.starts_with("notifications:") { n >= NOTIFICATION_FOLDER_AT } else { n > BULK_FOLDER_AT };
    let mut groups: Vec<(String, Vec<usize>)> = by_sender.into_iter().filter(|(k, v)| enough(k, v.len())).collect();
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (key, members) in groups {
        let (kind, sender) = key.split_once(':').unwrap_or(("marketing", &key));
        let parent = if kind == "notifications" { NOTIFICATIONS_FOLDER } else { MARKETING_FOLDER };
        let names: Vec<&str> = members.iter().filter_map(|&i| proposals[i].decision.sender_name.as_deref()).collect();
        let mut folder = bulk_folder_name(parent, &names, sender);
        if taken.contains(&folder.to_lowercase()) {
            folder = format!("{folder} ({sender})");
        }
        taken.push(folder.to_lowercase());
        for i in members {
            proposals[i].decision.action = Some(Action::Move { folder: folder.clone() });
            proposals[i].destination = Some(folder.clone());
        }
    }
}

/// Decides for one message: its headers' signals, its subject and the start of its text.
/// Decides without the message's full headers and text (exceptions and inclusions that test
/// them can't match): for tests and quick checks.
pub fn decide(s: &Signals, subject: &str, text: &str, ctx: &Context) -> Decision {
    decide_full(s, subject, text, None, ctx)
}

/// Every rule of the spam filter is checked through `Gate`: switched off, an exception that
/// matches, or an inclusion that makes it count although its own check didn't.
struct Gate<'a> {
    ctx: &'a Context,
    facts: Option<&'a Facts>,
}

impl Gate<'_> {
    /// None = the rule doesn't count; Some(false) = it does by its own check; Some(true) = only
    /// through an inclusion.
    fn hit(&self, key: &str, natural: bool) -> Option<bool> {
        let Some(set) = self.ctx.rule_settings.get(key) else { return natural.then_some(false) };
        if set.off {
            return None;
        }
        // a rule for some mailboxes only (a message whose mailbox isn't known counts everywhere)
        if !set.only_in.is_empty() && self.facts.is_some_and(|f| !f.personality.is_empty() && !set.only_in.iter().any(|m| m.eq_ignore_ascii_case(&f.personality))) {
            return None;
        }
        let any = |tests: &[Test]| self.facts.is_some_and(|f| tests.iter().any(|t| t.matches(f)));
        if natural {
            (!any(&set.exceptions)).then_some(false)
        } else {
            any(&set.inclusions).then_some(true)
        }
    }
}

pub fn decide_full(s: &Signals, subject: &str, text: &str, facts: Option<&Facts>, ctx: &Context) -> Decision {
    let gate = Gate { ctx, facts };
    // the message's mailbox: its own entries with the ones for every mailbox
    let mailbox = facts.map(|f| f.personality.to_lowercase()).unwrap_or_default();
    let lists = ctx.mailbox_lists.get(&mailbox).unwrap_or(&ctx.lists);
    let mut symbols: Vec<Symbol> = Vec::new();
    // a check that fired: its spam weight, and why (an inclusion says so instead)
    let mut check = |key: &'static str, natural: bool, score: f32, why: &dyn Fn() -> String| -> bool {
        match gate.hit(key, natural) {
            None => false,
            Some(included) => {
                let why = if included { format!("your inclusion for “{}”", rule_name(key)) } else { why() };
                symbols.push(Symbol { name: key, rule: key.to_string(), score, why });
                true
            }
        }
    };
    let allowed_by = (!s.from.is_empty()).then(|| lists.allowed_match(&s.from)).flatten();
    let entry_ok = allowed_by.as_ref().is_some_and(|p| gate.hit(&format!("allow:{p}"), true).is_some());
    let whitelisted = check("whitelist", entry_ok, -6.0, &|| {
        format!("{} is on your whitelist ({})", s.from, if lists.is_chosen(&s.from) { "you added them" } else { "you've written to them" })
    });
    // the server's own verdicts (Microsoft 365, other filters)
    let sfv = s.sfv.as_deref();
    check("m365-spam", sfv == Some("SPM"), 6.0, &|| "Microsoft 365 marked it as spam".into());
    check("m365-blocked", sfv == Some("BLK"), 6.0, &|| "the sender is on your Outlook blocked senders list".into());
    check("m365-allowed", matches!(sfv, Some("SKS" | "SKA" | "SFE")), -2.0, &|| "an Outlook allow list or rule let it through".into());
    check("m365-scl", s.scl.is_some_and(|scl| scl >= 5), 2.0, &|| format!("Microsoft 365's spam level is {} (5 or more = spam)", s.scl.unwrap_or(0)));
    check("spam-flag", s.spam_flag, 5.0, &|| "a spam filter on the way flagged it (X-Spam-Flag / X-Spam-Status)".into());
    // authentication
    let forwarded = s.list_id.is_some();
    check("dmarc-fail", s.dmarc.as_deref() == Some("fail"), if forwarded { 1.0 } else { 3.5 }, &|| {
        format!("it claims to be from {} but that domain's DMARC check failed{}", s.from_domain, if forwarded { " (a mailing list can cause this)" } else { "" })
    });
    check("spf-fail", s.spf.as_deref() == Some("fail"), if forwarded { 0.3 } else { 1.0 }, &|| "the sending server isn't allowed to send for that domain (SPF fail)".into());
    check("dkim-fail", s.dkim.as_deref() == Some("fail"), 0.5, &|| "its signature (DKIM) didn't check out".into());
    // claiming a well-known sender (Amazon, DocuSign, PayPal…) while its checks failed: phishing
    let forged = crate::triage_known::forged(s);
    check("forged-known", forged.is_some(), 4.0, &|| format!("it claims to be {} ({}) but its signatures don't check out: possibly phishing", forged.unwrap_or("?"), s.from));
    // fraud: a brand in the sender's name or subject that isn't who sent it, lures, the user's
    // own company's name from outside (`phishing.rs`; people the user writes to excepted)
    let fraud = crate::phishing::check(s, subject, text, &ctx.own_domains, whitelisted);
    check("phishing", fraud.iter().any(|f| f.rule == "phishing"), fraud.iter().filter(|f| f.rule == "phishing").map(|f| f.score).sum(), &|| {
        fraud.iter().filter(|f| f.rule == "phishing").map(|f| f.why.clone()).collect::<Vec<_>>().join("; ")
    });
    check("phishing-lure", fraud.iter().any(|f| f.rule == "phishing-lure"), 2.0, &|| fraud.iter().find(|f| f.rule == "phishing-lure").map(|f| f.why.clone()).unwrap_or_default());
    // bulk markers (light spam weight)
    check("bulk-service", s.esp.is_some(), 0.3, &|| format!("it was sent through {}, a bulk-mail service", s.esp.as_deref().unwrap_or("?")));
    let bcl = s.bcl.unwrap_or(0);
    check("m365-bulk", bcl >= 4, if bcl >= 8 { 1.5 } else { 0.5 }, &|| format!("Microsoft 365 rates it bulk mail (BCL {bcl}{})", if bcl >= 8 { ": often complained about" } else { "" }));
    // bulk-looking mail without the unsubscribe big senders must offer (Gmail/Yahoo 2024)
    let marketing_service = s.esp.as_deref().is_some_and(|e| MARKETING_SERVICES.iter().any(|m| e.starts_with(m)));
    check("bulk-no-unsubscribe", (marketing_service || bcl >= 4) && !s.unsubscribe, 1.0, &|| "it looks like marketing but has no unsubscribe link".into());
    // a character set for a writing system the user doesn't read: a hint, never the decision
    let charset = foreign_charset(&s.charsets, &ctx.languages);
    check("foreign-charset", charset.is_some(), 1.0, &|| {
        let (c, script) = charset.unwrap_or(("?", "?"));
        format!("it's encoded as {c}, a character set for {script} (a hint only)")
    });
    // what makes it bulk (facts shown under the Bulk rule)
    let mut markers = Vec::new();
    if s.unsubscribe {
        markers.push(format!("it has an unsubscribe link{}", if s.one_click { " (one-click)" } else { "" }));
    }
    if let Some(p) = &s.precedence {
        markers.push(format!("it's marked “Precedence: {p}”"));
    }
    if let Some(a) = &s.auto_submitted {
        markers.push(format!("it's automated (Auto-Submitted: {a})"));
    }
    if s.no_reply {
        markers.push(format!("{} is a no-reply address", s.from));
    }
    // a system's subject counts only for senders who aren't people you correspond with
    let machine_subject = s.machine_subject && !whitelisted;
    if s.machine {
        markers.push(format!("{} is a system's address, not a person's", s.from));
    } else if machine_subject {
        markers.push(format!("its subject reads like a system's (“{}”)", subject.chars().take(40).collect::<String>()));
    }
    if s.marketer {
        markers.push(format!("{} is a marketing address", s.from));
    }
    let marks = s.list_id.is_some() || s.unsubscribe || s.esp.is_some() || s.precedence.is_some() || s.auto_submitted.is_some() || bcl >= 4 || s.no_reply || s.machine || machine_subject || s.marketer;
    // two kinds: marketing ("bulk", the rule's old key) and notifications
    // a known sender's address, else the subject's words, beat the marks (Starlink's offers
    // have no unsubscribe link, Reddit's replies do: triage_known)
    let known_on = gate.hit("known-senders", true).is_some();
    let words_on = gate.hit("subject-words", true).is_some();
    // a verified well-known sender is machine-sent mail even without the marks; the subject's
    // words only sort what's machine-sent (never a person's own mail)
    let verified_known = (known_on && crate::triage_known::verified(s)).then(|| crate::triage_known::known(&s.from)).flatten();
    let bulk_natural = marks || verified_known.is_some();
    let hinted = if bulk_natural { crate::triage_known::hint(s, subject, known_on, words_on) } else { None };
    let marketing_natural = match &hinted {
        Some(h) => bulk_natural && h.kind == crate::triage_known::Kind::Marketing,
        None => bulk_natural && (is_marketing(s) || s.marketer),
    };
    let hint_why = hinted.as_ref().map(|h| (h.rule, h.why.clone()));
    let sender = bulk_sender_for(s, &ctx.own_domains);
    let marketing = gate.hit("bulk", marketing_natural);
    let notification = if marketing.is_some() { None } else { gate.hit("notifications", bulk_natural && !marketing_natural) };
    let bulkish = marketing.is_some() || notification.is_some();
    let is_list = gate.hit("list", s.list_id.is_some());
    // what this mailbox has learned (a message whose mailbox has learned enough)
    let learned = facts.and_then(|f| ctx.bayes.get(&f.personality.to_lowercase()).map(|m| m.score(&crate::bayes::tokens(f))));
    if let Some(score) = learned {
        let pct = (score * 100.0).round();
        if !whitelisted {
            // mass mail that proves where it's from counts for less: Junk folders hold much of
            // it (put there by Microsoft), but bulk isn't spam; it goes to Bulk unless
            // something else says spam
            let proven_bulk = bulkish && crate::triage_known::verified(s);
            let weight = if proven_bulk { 1.5 } else if score > 0.99 { 5.0 } else { 3.5 };
            check("learned", score > crate::bayes::SPAM_ABOVE, weight, &|| {
                format!("what this mailbox has learned says spam ({pct}% sure){}", if proven_bulk { "; counted less: it's mass mail that proves where it's from" } else { "" })
            });
        }
        check("learned-good", score < crate::bayes::HAM_BELOW, -1.5, &|| format!("what this mailbox has learned says good mail ({}% spam)", pct));
    }
    // a domain registered only days ago (looked up for new mail)
    let org = org_domain(&s.from_domain);
    // the blocklists (checked for new mail)
    if let (Some(f), false) = (facts, whitelisted) {
        for (rule, weight, why) in crate::blocklists::listings_for(&f.headers, &org, &ctx.listings) {
            check(rule, true, weight, &|| why.clone());
        }
    }
    if let Some(reg) = ctx.domains.get(&org).and_then(|d| d.registered) {
        let age = crate::library::now() - reg;
        if !whitelisted && age >= 0 {
            let days = age / 86_400;
            check("young-domain", age < crate::reputation::YOUNG, if age < crate::reputation::VERY_YOUNG { 3.5 } else { 2.0 }, &|| format!("{org} was registered {days} day{} ago", if days == 1 { "" } else { "s" }));
        }
    }
    let spam_score: f32 = symbols.iter().map(|x| x.score).sum();

    // priority phrases, with safeguards
    let hay = format!("{subject}\n{}", text.chars().take(2000).collect::<String>());
    let mut priority = false;
    let found = ctx.phrases.find(&hay);
    let trusted_sender = whitelisted || s.dmarc.as_deref() == Some("pass") || s.dkim.as_deref() == Some("pass");
    for (group, _) in &ctx.phrases_groups {
        let key = format!("phrases:{group}");
        let phrase = found.iter().find(|(g, _)| g == group).map(|(_, p)| p.clone());
        let Some(included) = gate.hit(&key, phrase.is_some()) else { continue };
        let rule = key.clone();
        if included {
            priority = true;
            symbols.push(Symbol { name: "PRIORITY_PHRASE", rule, score: 0.0, why: format!("your inclusion for “{group}”") });
            continue;
        }
        let phrase = phrase.unwrap_or_default();
        let upset = group.starts_with("Upset");
        if s.dmarc.as_deref() == Some("fail") {
            symbols.push(Symbol { name: "SUSPICIOUS_NOTICE", rule, score: 2.0, why: format!("it says “{phrase}” but its sender doesn't check out: possibly phishing") });
        } else if upset && bulkish && !whitelisted {
            // marketing copy ("don't be disappointed!") isn't an upset customer
        } else if (marketing.is_some() || (bulkish && !whitelisted)) && !subject.to_lowercase().contains(&phrase.to_lowercase()) {
            // marketing (from anyone) and mass mail from strangers: a real notice says so in its
            // subject ("Disconnect notice for account 1234"); the same words deep in a newsletter
            // or marketing copy don't count
        } else if bulkish && !trusted_sender {
            symbols.push(Symbol { name: "NOTICE_UNVERIFIED", rule, score: 0.0, why: format!("it says “{phrase}”, but it's bulk mail from a sender that couldn't be verified") });
        } else {
            priority = true;
            symbols.push(Symbol { name: "PRIORITY_PHRASE", rule, score: 0.0, why: format!("{group}: it says “{phrase}”") });
        }
    }
    let spam_score: f32 = spam_score + symbols.iter().filter(|x| x.name == "SUSPICIOUS_NOTICE").map(|x| x.score).sum::<f32>();

    // the decision, in order; the rule that decided goes first in the list
    let mut decided = |name: &'static str, rule: String, why: String| symbols.insert(0, Symbol { name, rule, score: 0.0, why });
    let blocked = lists.blocked_entries(&s.from).into_iter().find_map(|(pattern, action, hit)| {
        gate.hit(&format!("block:{pattern}"), hit).map(|inc| (pattern, action, inc))
    });
    // security advisories and patch notices stay in the Inbox, unless a fraud check fired (a bare mention of a brand, +1, is no fraud)
    let security = gate.hit("security-notice", fraud.iter().map(|f| f.score).sum::<f32>() < 2.0 && crate::views::security_notice(subject, s, whitelisted, &ctx.own_domains, false));
    let fingerprint = Fingerprint::of(s, subject);
    let like = ctx.rules.iter().filter(|r| r.mailbox.is_empty() || r.mailbox == mailbox).find_map(|r| gate.hit(&format!("like:{}", r.id), r.enabled && r.keys.matches(&fingerprint)).map(|inc| (r, inc)));
    // the subject on its own too: an English footer (a Google Groups list's "You received this
    // message because…") mustn't outweigh an Arabic subject and text
    let foreign = foreign_subject(subject, &ctx.languages)
        .or_else(|| foreign_writing(&body_without_footer(text), &ctx.languages))
        .or_else(|| foreign_writing(&hay, &ctx.languages));
    let (category, action) = if let Some((pattern, action, inc)) = blocked {
        decided("BLACKLIST", format!("block:{pattern}"), if inc { format!("your inclusion for the blacklist entry {pattern}") } else { format!("{} is blacklisted ({pattern})", s.from) });
        (Category::Blocked, Some(action.clone()))
    } else if let Some(inc) = security {
        decided("SECURITY_NOTICE", "security-notice".into(), if inc { "your inclusion for “Security notices”".into() } else { "a security advisory or patch notice from a sender that proves who it is: it stays in the Inbox".into() });
        (Category::Priority, None)
    } else if let Some((rule, inc)) = like {
        decided("LIKE_RULE", format!("like:{}", rule.id), if inc { format!("your inclusion for your rule “{}”", rule.name) } else { format!("your rule “{}”: {}", rule.name, rule.keys.describe()) });
        (Category::Rule, Some(rule.action.clone()))
    } else if let Some(inc) = gate.hit("other-language", foreign.is_some() && !(whitelisted && !bulkish)) {
        // not in a language the user reads (people they correspond with are left alone)
        let why = match foreign {
            Some(written) if !inc => format!("it's written in {written}, not in your languages ({})", language_names(&ctx.languages)),
            _ => "your inclusion for “Other languages”".to_string(),
        };
        decided("OTHER_LANGUAGE", "other-language".into(), why);
        (Category::Spam, Some(Action::Move { folder: OTHER_LANGUAGES_FOLDER.into() }))
    } else if let Some(inc) = gate.hit("spam", spam_score >= 5.0 && !whitelisted) {
        decided("SPAM", "spam".into(), if inc { "your inclusion for “Spam”".into() } else { format!("its spam score is {spam_score:.1} (5 or more = spam)") });
        (Category::Spam, Some(Action::Spam))
    } else if priority {
        (Category::Priority, None)
    } else if whitelisted && (!bulkish || lists.is_chosen(&s.from)) {
        // a person you correspond with; a mass mailing only from senders you added yourself
        // (replying to one Zoom or Amazon message mustn't bring all their marketing back in)
        (Category::People, None)
    } else if let Some(inc) = is_list {
        decided("LIST", "list".into(), if inc { "your inclusion for “Mailing lists”".into() } else { format!("it's from the mailing list {}", s.list_id.as_deref().unwrap_or("?")) });
        (Category::List, Some(Action::Move { folder: "Lists".into() }))
    } else if let Some(inc) = marketing {
        let (rule, why) = match (&hint_why, inc) {
            (_, true) => ("bulk", "your inclusion for “Marketing”".to_string()),
            (Some((rule, why)), false) => (*rule, why.clone()),
            (None, false) => ("bulk", markers.first().cloned().unwrap_or_else(|| "it has the marks of a mass mailing".into())),
        };
        decided("MARKETING", rule.into(), why);
        (Category::Bulk, Some(Action::Move { folder: bulk_destination(Category::Bulk, &sender, &ctx.bulk_folders) }))
    } else if let Some(inc) = notification {
        let (rule, why) = match (&hint_why, inc) {
            (_, true) => ("notifications", "your inclusion for “Notifications”".to_string()),
            (Some((rule, why)), false) => (*rule, why.clone()),
            (None, false) => (
                "notifications",
                format!("it's sent by a machine ({}), without an unsubscribe link: a notification", markers.first().cloned().or_else(|| s.esp.clone().map(|e| format!("through {e}"))).unwrap_or_else(|| "automated".into())),
            ),
        };
        decided("NOTIFICATION", rule.into(), why);
        (Category::Notification, Some(Action::Move { folder: bulk_destination(Category::Notification, &sender, &ctx.bulk_folders) }))
    } else {
        (Category::Other, None)
    };
    let mark_rule = if notification.is_some() { "notifications" } else { "bulk" };
    for m in markers {
        if !symbols.iter().any(|x| x.why == m || x.why.contains(&m)) {
            symbols.push(Symbol { name: "BULK_MARK", rule: mark_rule.into(), score: 0.0, why: m });
        }
    }
    let (sender_key, sender_name) = if matches!(category, Category::Bulk | Category::Notification) {
        (Some(sender_key(category, &sender)), Some(s.from_name.clone()))
    } else {
        (None, None)
    };
    Decision { category, priority: (priority || security.is_some()) && !matches!(category, Category::Spam | Category::Blocked), action, spam_score, symbols, sender_key, sender_name }
}

/// The spam filter's own rules: (key, name, what it does). Each can be switched off, and given
/// exceptions and inclusions (`RuleSettings`).
pub const BUILT_IN_RULES: [(&str, &str, &str); 31] = [
    ("spamhaus-ip", "Spamhaus: sending server", "The server that sent it is on Spamhaus's ZEN list (+4; a home or dynamic address, PBL: +1.5). New mail only, with your Spamhaus DQS key."),
    ("spamhaus-domain", "Spamhaus: domain", "The sender's domain is on Spamhaus's domain list, DBL: spam, phishing or malware (+5). New mail only, with your key."),
    ("spamhaus-new-domain", "Spamhaus: brand-new domain", "The sender's domain was first seen on the Internet in the last 24 hours, ZRD (+2.5). New mail only, with your key."),
    ("young-domain", "Young domain", "The sender's domain was registered in the last 30 days (+2; in the last week +3.5), unless you write to them. Looked up for new mail (the public domain registries, RDAP)."),
    ("learned", "Learned: spam", "What this mailbox's learning says is spam (over 90% sure: +3.5; over 99%: +5; mass mail that proves where it's from: +1.5). It learns from the Junk / Spam folders and every Spam you mark."),
    ("learned-good", "Learned: good mail", "What this mailbox's learning says is good mail (under 20%: −1.5). It learns from people you write to and every Not Spam you mark."),
    ("whitelist", "Whitelist", "People you've written to, and senders you added, are never spam and stay in the Inbox (their mass mail only if you added them)."),
    ("m365-spam", "Microsoft 365 says spam", "Microsoft 365 marked it as spam (spam score +6)."),
    ("m365-blocked", "Outlook blocked senders", "The sender is on your Outlook blocked senders list (+6)."),
    ("m365-allowed", "Outlook allow lists", "An Outlook allow list or rule let it through (−2)."),
    ("m365-scl", "Microsoft 365 spam level", "Microsoft 365's spam confidence level is 5 or more (+2)."),
    ("spam-flag", "Other spam filters", "A spam filter on the way flagged it: X-Spam-Flag / X-Spam-Status (+5)."),
    ("dmarc-fail", "DMARC failed", "It claims a domain whose DMARC check failed (+3.5; +1 for mailing lists)."),
    ("spf-fail", "SPF failed", "The sending server isn't allowed to send for its domain (+1)."),
    ("forged-known", "Fake well-known sender", "It claims to be Amazon, PayPal, DocuSign… but its signatures don't check out: possibly phishing (+4)."),
    ("phishing", "Fraud (phishing)", "It names a brand (DocuSign, Microsoft, PayPal, a bank…) in its sender name, or in its subject with a lure, but doesn't come from that brand; or it uses your organization's name from outside (+3 to +6)."),
    ("phishing-lure", "Fraud lures", "It asks for what frauds do (sign a document, reset a password, a full mailbox, an unusual sign-in) and its sender isn't proven (+2)."),
    ("dkim-fail", "DKIM failed", "Its signature didn't check out (+0.5)."),
    ("bulk-service", "Sent through a bulk-mail service", "SendGrid, Mailchimp, Klaviyo… (+0.3)."),
    ("m365-bulk", "Microsoft 365 bulk rating", "Microsoft 365 rates it bulk mail: BCL 4 or more (+0.5; 8 or more +1.5)."),
    ("bulk-no-unsubscribe", "Marketing without unsubscribe", "Rated bulk by Microsoft, or sent by a marketing service, but no unsubscribe link (+1)."),
    ("foreign-charset", "Foreign character set", "Encoded in a character set for a writing system you don't read (Chinese GB2312, Russian KOI8-R…): a hint only (+1); the words themselves decide the language."),
    ("security-notice", "Security notices", "Security advisories and patch notices (CISA, vendors' PSIRT lists, “security update”, “known exploited vulnerability”, CVE…) from a sender that proves who it is, or forwarded by someone you write to or a colleague: stay in the Inbox and are a Red Alert. Not a security product's or SOC service's detections, not marketing about security."),
    ("spam", "Spam", "A spam score of 5 or more (and not whitelisted): to Spam."),
    ("other-language", "Other languages", "Written in a language that isn't one of yours: to Spam."),
    ("list", "Mailing lists", "Has a List-Id: to Lists."),
    ("bulk", "Marketing", "Mass mailings (an unsubscribe link, Precedence: bulk, a marketing service, Microsoft's bulk rating): to Bulk/Marketing (a mailbox of its own over 100 messages)."),
    ("notifications", "Notifications", "Machine-sent mail about you, without an unsubscribe link (orders, alerts, receipts, signatures, tickets): to Bulk/Notifications/<sender> (from 10 messages; rare senders share Bulk/Notifications)."),
    ("known-senders", "Well-known senders", "Eudora knows which addresses of Amazon, DocuSign, Jira, LinkedIn, GitHub, UPS… send notifications and which send marketing, and names their folders."),
    ("subject-words", "Subject words", "For senders that send both kinds (Starlink, Reddit, Zoom, Nextdoor…): “shipped”, “invoice”, “replied to” mean a notification; “free”, “% off”, “webinar” mean marketing."),
    ("people", "People you correspond with", "(Shown for completeness: see Whitelist.)"),
];

fn rule_name(key: &str) -> &str {
    BUILT_IN_RULES.iter().find(|r| r.0 == key).map(|r| r.1).unwrap_or(key)
}

/// A rule's switch, exceptions and inclusions (setting "triage-rule-settings", by rule key:
/// a built-in key, "phrases:<group>", "block:<pattern>", "allow:<pattern>", "like:<id>").
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleSettings {
    /// Ignored: as if it weren't there.
    pub off: bool,
    /// The rule doesn't count for messages matching any of these.
    pub exceptions: Vec<Test>,
    /// The rule counts for messages matching any of these, even when its own check doesn't.
    pub inclusions: Vec<Test>,
    /// Only in these mailboxes (account addresses, lowercase); empty = every mailbox
    pub only_in: Vec<String>,
}

/// One row of the Spam Filter Rules list.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FilterRule {
    pub key: String,
    /// "built-in", "phrases", "language", "block", "allow", "like".
    pub kind: String,
    pub name: String,
    pub description: String,
    pub settings: RuleSettings,
}

/// Where a whitelist / blacklist pattern applies, for the rules list: "" (every mailbox), or
/// " · in a@x.com, b@y.com only".
fn entry_mailboxes(entries: &[crate::triage::SenderRule], kind: &str, pattern: &str) -> String {
    let all: Vec<&str> = entries.iter().filter(|e| e.kind == kind && e.pattern == pattern).map(|e| e.mailbox.as_str()).collect();
    if all.iter().any(|m| m.is_empty()) {
        String::new()
    } else {
        format!(" · in {} only", all.join(", "))
    }
}

// ------------------------------------------------------------------ the dry run and applying

/// One message's proposal in a dry run.
#[derive(Clone, Debug, Serialize)]
pub struct Proposal {
    pub id: i64,
    pub date: i64,
    pub from: String,
    pub subject: String,
    /// The folder it's in (where it would be moved from).
    pub folder: i64,
    pub decision: Decision,
    /// Where it would go ("Lists", "Trash"…); None = stays.
    pub destination: Option<String>,
}

/// A message read for deciding: (id, folder, date, from, subject, where its bytes are).
type Row = (i64, i64, i64, String, String, crate::pack::Location);

/// A proposal the user kept (or one carried out as is): what to do with one message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chosen {
    pub id: i64,
    /// The folder it's in now.
    pub folder: i64,
    pub category: String,
    #[serde(default)]
    pub priority: bool,
    /// None = it stays.
    pub action: Option<Action>,
    #[serde(default)]
    pub sender_key: Option<String>,
    #[serde(default)]
    pub reasons: Vec<String>,
    /// Stays, but the spam filter wasn't sure: listed in the Maybe Spam view.
    #[serde(default)]
    pub maybe: bool,
}

impl From<&Proposal> for Chosen {
    fn from(p: &Proposal) -> Chosen {
        Chosen {
            id: p.id,
            folder: p.folder,
            category: p.decision.category.name().to_string(),
            priority: p.decision.priority,
            action: p.decision.action.clone(),
            sender_key: p.decision.sender_key.clone(),
            reasons: p.decision.symbols.iter().map(|s| s.why.clone()).collect(),
            maybe: p.decision.is_maybe(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Preview {
    pub proposals: Vec<Proposal>,
    /// How many per category.
    pub counts: HashMap<String, u64>,
    pub looked_at: u64,
}

fn destination(action: &Option<Action>) -> Option<String> {
    match action {
        None => None,
        Some(Action::Move { folder }) => Some(folder.clone()),
        Some(Action::Trash) => Some("Trash".into()),
        Some(Action::Spam) => Some(SPAM_FOLDER.into()),
        Some(Action::Label { label }) => Some(format!("(label {label})")),
    }
}

const RULES_SETTING: &str = "triage-like-rules";
const PHRASES_SETTING: &str = "triage-phrases";
const BULK_FOLDERS_SETTING: &str = "triage-bulk-folders";
/// The mailboxes the spam filter works in (account addresses, lowercase; JSON list). Unset =
/// the default (`FilterMailbox::default_on`).
const MAILBOXES_SETTING: &str = "triage-mailboxes";

/// A mailbox the spam filter may work in (Spam Filter Rules ▸ Mailboxes).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FilterMailbox {
    /// Its accounts: usually one; an address can have several (its IMAP account and an old
    /// classic Eudora archive of it), chosen together, as rules name a mailbox by address.
    pub accounts: Vec<i64>,
    /// The account's address, lowercase (how rules name it).
    pub address: String,
    /// How the account is shown (its name, else its address).
    pub shown: String,
    pub kind_name: String,
    /// The filter sorts arriving mail here, and the dry run looks here.
    pub on: bool,
}

impl FilterMailbox {
    /// On unless chosen otherwise: the user's own accounts (Microsoft 365, IMAP, POP3); not
    /// shared mailboxes (other people's mail) or imported archives.
    pub fn default_on(kind: &str) -> bool {
        matches!(kind, "graph" | "imap" | "pop3" | "gmail")
    }
}
const LANGUAGES_SETTING: &str = "triage-languages";
const RULE_SETTINGS: &str = "triage-rule-settings";

/// An action in plain words.
pub fn describe_action(a: &Action) -> String {
    match a {
        Action::Move { folder } => format!("move to {folder}"),
        Action::Trash => "move to Trash".into(),
        Action::Spam => "move to Spam".into(),
        Action::Label { label } => format!("label it {label}"),
    }
}

impl Library {
    pub fn like_rules(&self) -> Result<Vec<LikeRule>> {
        Ok(self.setting(RULES_SETTING)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    pub fn set_like_rules(&self, rules: &[LikeRule]) -> Result<()> {
        self.set_setting(RULES_SETTING, &serde_json::to_string(rules)?)
    }

    /// The organizations the user's own addresses belong to (never a shared mail service).
    pub fn own_org_domains(&self) -> Result<Vec<String>> {
        let mut d: Vec<String> = self.own_addresses()?.iter().map(|a| org_domain(a.split('@').nth(1).unwrap_or(""))).filter(|d| !d.is_empty() && !shared(d)).collect();
        d.sort();
        d.dedup();
        Ok(d)
    }

    pub fn priority_phrases(&self) -> Result<Vec<(String, Vec<String>)>> {
        Ok(self.setting(PHRASES_SETTING)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_else(default_phrases))
    }

    pub fn set_priority_phrases(&self, groups: &[(String, Vec<String>)]) -> Result<()> {
        self.set_setting(PHRASES_SETTING, &serde_json::to_string(groups)?)
    }

    pub fn triage_context(&self) -> Result<Context> {
        self.triage_context_for(None)
    }

    /// The context for messages in these mailboxes only (their addresses): each mailbox's own
    /// lists and learned model are loaded only when one of its messages is decided (sorting two
    /// messages from one mailbox read every mailbox's model: slow).
    pub fn triage_context_for(&self, only: Option<&[String]>) -> Result<Context> {
        let wanted = |address: &str| only.is_none_or(|list| list.iter().any(|m| m.eq_ignore_ascii_case(address)));
        let groups = self.priority_phrases()?;
        Ok(Context {
            lists: self.sender_lists()?,
            mailbox_lists: self.mailbox_lists()?.into_iter().filter(|(m, _)| wanted(m)).collect(),
            rules: self.like_rules()?,
            phrases: Phrases::new(&groups),
            bulk_folders: self.bulk_folders()?,
            languages: self.primary_languages()?,
            phrases_groups: groups,
            rule_settings: self.rule_settings()?,
            own_domains: self.own_org_domains()?,
            listings: self.blocklist_listings()?,
            domains: {
                let mut stmt = self.conn.prepare("SELECT domain, registered, first_seen FROM domains")?;
                let rows: Vec<(String, Option<i64>, Option<i64>)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
                rows.into_iter().map(|(d, registered, first_seen)| (d, crate::reputation::DomainFacts { registered, first_seen })).collect()
            },
            bayes: {
                let mut models = HashMap::new();
                let mut stmt = self.conn.prepare("SELECT DISTINCT b.account, lower(a.address) FROM bayes_trained b JOIN accounts a ON a.id = b.account WHERE a.address IS NOT NULL")?;
                let accounts: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
                for (id, address) in accounts.into_iter().filter(|(_, a)| wanted(a)) {
                    let model = self.bayes_model(id)?;
                    if model.ready() {
                        models.insert(address, std::sync::Arc::new(model));
                    }
                }
                models
            },
        })
    }

    /// Every account with an address, and whether the spam filter works in it.
    pub fn spam_filter_mailboxes(&self) -> Result<Vec<FilterMailbox>> {
        let chosen: Option<Vec<String>> = self.setting(MAILBOXES_SETTING)?.and_then(|j| serde_json::from_str(&j).ok());
        let mut out: Vec<FilterMailbox> = Vec::new();
        for a in self.accounts_counted(false)? {
            let Some(address) = a.address.clone().map(|x| x.to_lowercase()) else { continue };
            let on = match &chosen {
                Some(list) => list.contains(&address),
                None => FilterMailbox::default_on(&a.kind),
            };
            match out.iter_mut().find(|m| m.address == address) {
                Some(m) => {
                    m.accounts.push(a.id);
                    m.kind_name = format!("{} + {}", m.kind_name, a.kind_name);
                    m.on |= on;
                }
                None => out.push(FilterMailbox { accounts: vec![a.id], shown: a.shown(), kind_name: a.kind_name.to_string(), address, on }),
            }
        }
        Ok(out)
    }

    /// Chooses the mailboxes the spam filter works in (addresses).
    pub fn set_spam_filter_mailboxes(&self, addresses: &[String]) -> Result<()> {
        let mut list: Vec<String> = addresses.iter().map(|a| a.trim().to_lowercase()).filter(|a| !a.is_empty()).collect();
        list.sort();
        list.dedup();
        self.set_setting(MAILBOXES_SETTING, &serde_json::to_string(&list)?)
    }

    /// The ids of the accounts the spam filter works in.
    pub fn spam_filter_accounts(&self) -> Result<std::collections::HashSet<i64>> {
        Ok(self.spam_filter_mailboxes()?.into_iter().filter(|m| m.on).flat_map(|m| m.accounts).collect())
    }

    /// The Inboxes of the mailboxes the spam filter works in (the dry run's default).
    pub fn spam_filter_inboxes(&self) -> Result<Vec<i64>> {
        let accounts = self.spam_filter_accounts()?;
        let mut stmt = self.conn.prepare("SELECT id, account FROM folders WHERE lower(path) = 'inbox'")?;
        let rows: Vec<(i64, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows.into_iter().filter(|(_, a)| accounts.contains(a)).map(|(f, _)| f).collect())
    }

    /// The mailbox (account address, lowercase) of each folder: what a per-mailbox rule tests.
    /// The mailboxes (account addresses) these folders are in.
    fn mailboxes_of_folders(&self, folders: &[i64]) -> Result<Vec<String>> {
        let all = self.folder_mailboxes()?;
        let mut out: Vec<String> = folders.iter().filter_map(|f| all.get(f).cloned()).collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn folder_mailboxes(&self) -> Result<HashMap<i64, String>> {
        let mut stmt = self.conn.prepare("SELECT f.id, lower(COALESCE(a.address, '')) FROM folders f JOIN accounts a ON a.id = f.account")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Every rule's switch, exceptions and inclusions.
    pub fn rule_settings(&self) -> Result<HashMap<String, RuleSettings>> {
        Ok(self.setting(RULE_SETTINGS)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    /// Sets one rule's switch, exceptions and inclusions (all default = removed).
    pub fn set_rule_settings(&self, key: &str, settings: &RuleSettings) -> Result<()> {
        let mut all = self.rule_settings()?;
        if *settings == RuleSettings::default() {
            all.remove(key);
        } else {
            all.insert(key.to_string(), settings.clone());
        }
        self.set_setting(RULE_SETTINGS, &serde_json::to_string(&all)?)
    }

    /// The Mail Filter Rules list, in the order they're applied: blacklist entries, your "like
    /// this" rules, the built-in rules (whitelist entries you added just after the whitelist),
    /// the priority phrase groups. The automatic whitelist is one row ("Whitelist"), not
    /// thousands.
    pub fn spam_filter_rules(&self) -> Result<Vec<FilterRule>> {
        let settings = self.rule_settings()?;
        let row = |key: String, kind: &str, name: String, description: String| FilterRule {
            settings: settings.get(&key).cloned().unwrap_or_default(),
            key,
            kind: kind.to_string(),
            name,
            description,
        };
        let mut out = Vec::new();
        let entries = self.sender_rules()?;
        // one row per pattern (its rule key), naming the mailboxes it applies in
        let mut seen = std::collections::HashSet::new();
        for e in entries.iter().filter(|e| e.kind == "block") {
            if !seen.insert(("block", e.pattern.clone())) {
                continue;
            }
            let action = describe_action(e.action.as_ref().unwrap_or(&Action::default()));
            let note = if e.note.is_empty() { String::new() } else { format!(" ({})", e.note) };
            let r#where = entry_mailboxes(&entries, "block", &e.pattern);
            out.push(row(format!("block:{}", e.pattern), "block", format!("Blacklist: {}", e.pattern), format!("{action}{note}{where}")));
        }
        for r in self.like_rules()? {
            let state = if r.enabled { "" } else { " (made inactive)" };
            let r#where = if r.mailbox.is_empty() { String::new() } else { format!(" · in {} only", r.mailbox) };
            out.push(row(format!("like:{}", r.id), "like", r.name.clone(), format!("Mail with {}: {}{state}{where}", r.keys.describe(), describe_action(&r.action))));
        }
        for (key, name, what) in BUILT_IN_RULES.iter().filter(|r| r.0 != "people") {
            let kind = if *key == "other-language" { "language" } else { "built-in" };
            out.push(row(key.to_string(), kind, name.to_string(), what.to_string()));
            if *key == "whitelist" {
                for e in entries.iter().filter(|e| e.kind == "allow" && e.source == "user") {
                    if !seen.insert(("allow", e.pattern.clone())) {
                        continue;
                    }
                    let note = if e.note.is_empty() { "added by you".to_string() } else { e.note.clone() };
                    let r#where = entry_mailboxes(&entries, "allow", &e.pattern);
                    out.push(row(format!("allow:{}", e.pattern), "allow", format!("Whitelist: {}", e.pattern), format!("{note}{where}")));
                }
            }
        }
        for (group, phrases) in self.priority_phrases()? {
            out.push(row(format!("phrases:{group}"), "phrases", format!("High priority: {group}"), phrases.join(", ")));
        }
        Ok(out)
    }

    /// Inserts a "messages like this one" rule (from a message, or keys written by hand).
    /// Returns its id.
    pub fn add_like_rule(&self, name: &str, keys: Fingerprint, action: Action) -> Result<i64> {
        self.add_like_rule_in(name, keys, action, "")
    }

    /// The same, in one mailbox only (its account's address; empty = every mailbox).
    pub fn add_like_rule_in(&self, name: &str, keys: Fingerprint, action: Action, mailbox: &str) -> Result<i64> {
        if keys == Fingerprint::default() {
            return Err(Error::Other("a rule needs something to look at (a mailing list, a domain, an address or a subject)".into()));
        }
        let mut rules = self.like_rules()?;
        let id = rules.iter().map(|r| r.id).max().unwrap_or(0) + 1;
        rules.push(LikeRule { id, name: name.to_string(), keys, action, created: now(), enabled: true, mailbox: mailbox.trim().to_lowercase() });
        self.set_like_rules(&rules)?;
        Ok(id)
    }

    pub fn remove_like_rule(&self, id: i64) -> Result<()> {
        let mut rules = self.like_rules()?;
        rules.retain(|r| r.id != id);
        self.set_like_rules(&rules)?;
        self.set_rule_settings(&format!("like:{id}"), &RuleSettings::default())
    }

    /// The user's primary languages (setting "triage-languages"; English until chosen).
    pub fn primary_languages(&self) -> Result<Vec<String>> {
        Ok(self.setting(LANGUAGES_SETTING)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_else(|| vec!["en".to_string()]))
    }

    pub fn set_primary_languages(&self, codes: &[String]) -> Result<()> {
        for c in codes {
            if !LANGUAGES.iter().any(|l| l.0 == c) {
                return Err(Error::Other(format!("unknown language {c}")));
            }
        }
        self.set_setting(LANGUAGES_SETTING, &serde_json::to_string(codes)?)
    }

    /// "Not Spam": the sender goes on the whitelist (as the user's own entry, so even their
    /// mass mailings stay), and any blacklist entry or "like this" rule sending it to Junk or
    /// Trash is taken out. Returns what was done, for the user.
    pub fn mark_not_spam(&self, id: i64) -> Result<Vec<String>> {
        let raw = self.raw_message(id)?;
        let (s, subject, _) = Library::triage_input(&raw);
        let mut done = Vec::new();
        if s.from.is_empty() {
            return Ok(done);
        }
        // in the message's own mailbox; a
        // blacklist entry for this very address goes, wherever it applied
        let mailbox = self.message_mailbox(id)?;
        self.set_sender_rule_in(&s.from, "allow", None, "marked Not Spam", &mailbox)?;
        self.conn.execute("DELETE FROM sender_rules WHERE pattern = lower(?1) AND kind = 'block'", params![s.from])?;
        self.note_sender_rules()?;
        if let Some(account) = self.message_account(id)? {
            self.bayes_learn(&[(id, account)], false)?; // learned as good mail (a spam learned before is unlearned)
        }
        done.push(format!("{} added to your whitelist", s.from));
        let fp = Fingerprint::of(&s, &subject);
        let mut rules = self.like_rules()?;
        let before = rules.len();
        rules.retain(|r| !(matches!(r.action, Action::Spam | Action::Trash) && r.keys.matches(&fp) && (r.mailbox.is_empty() || r.mailbox == mailbox)));
        if rules.len() != before {
            done.push(format!("{} rule(s) that sent mail like this to Junk or Trash removed", before - rules.len()));
            self.set_like_rules(&rules)?;
        }
        self.conn.execute("UPDATE triage SET category = 'people', decided = ?2 WHERE message = ?1", params![id, now()])?;
        self.set_maybe(id, false)?;
        Ok(done)
    }

    /// "Spam": the sender is blacklisted to Junk, and a "like this" rule catches the same
    /// mailing from other addresses (its List-Id, signer or own domain; never a shared service
    /// domain alone, see `Fingerprint::rule_keys`). Returns what was done.
    pub fn mark_spam(&self, id: i64) -> Result<Vec<String>> {
        let raw = self.raw_message(id)?;
        let (s, subject, _) = Library::triage_input(&raw);
        let mut done = Vec::new();
        if let Some(account) = self.message_account(id)? {
            self.bayes_learn(&[(id, account)], true)?; // learned as spam
        }
        let mailbox = self.message_mailbox(id)?;
        if !s.from.is_empty() {
            self.set_sender_rule_in(&s.from, "block", Some(&Action::Spam), "marked Spam", &mailbox)?;
            done.push(format!("{} blacklisted (to Junk)", s.from));
        }
        let keys = Fingerprint::of(&s, &subject).rule_keys();
        let single_address = keys.from_address.as_deref() == Some(s.from.as_str()) && keys.list_id.is_none() && keys.feedback.is_none() && keys.dkim_org.is_none() && keys.from_org.is_none();
        if !single_address && keys != Fingerprint::default() {
            let mut rules = self.like_rules()?;
            if !rules.iter().any(|r| r.keys == keys && (r.mailbox.is_empty() || r.mailbox == mailbox)) {
                let id = rules.iter().map(|r| r.id).max().unwrap_or(0) + 1;
                let name = format!("Spam like “{}”", subject.chars().take(40).collect::<String>());
                done.push(format!("mail with {} goes to Junk too", keys.describe()));
                rules.push(LikeRule { id, name, keys, action: Action::Spam, created: now(), enabled: true, mailbox: mailbox.clone() });
                self.set_like_rules(&rules)?;
            }
        }
        self.conn.execute("UPDATE triage SET category = 'spam', decided = ?2 WHERE message = ?1", params![id, now()])?;
        self.set_maybe(id, false)?;
        Ok(done)
    }

    /// Spam / Not Spam on a selection (message, the folder it's in): each is marked (sender
    /// blacklisted or whitelisted, see `mark_spam` / `mark_not_spam`) and moved: Spam to the
    /// account's Spam folder, Not Spam back to its Inbox (when it isn't there). Returns (what was
    /// done, without repeats; how many moved).
    pub fn mark_and_move(&self, items: &[(i64, i64)], spam: bool) -> Result<(Vec<String>, u64)> {
        let mut notes: Vec<String> = Vec::new();
        let mut moved = 0;
        for (id, folder) in items {
            let done = if spam { self.mark_spam(*id)? } else { self.mark_not_spam(*id)? };
            for d in done {
                if !notes.contains(&d) {
                    notes.push(d);
                }
            }
            let account: i64 = self.conn.query_row("SELECT account FROM folders WHERE id = ?1", [folder], |r| r.get(0))?;
            let to = if spam { self.spam_folder(account)? } else { crate::db::ensure_folder(&self.conn, account, "Inbox")? };
            if to != *folder {
                moved += self.move_messages(&[*id], *folder, to)?;
            }
        }
        Ok((notes, moved))
    }

    /// One message's decision (`triage --id`), without reading a whole folder.
    pub fn triage_one(&self, id: i64) -> Result<Proposal> {
        let (date, from, subject, folder): (i64, String, String, i64) = self.conn.query_row(
            "SELECT m.date, m.from_display, m.subject, COALESCE((SELECT folder FROM locations WHERE message = m.id LIMIT 1), 0) FROM messages m WHERE m.id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let ctx = self.triage_context_for(Some(&self.mailboxes_of_folders(&[folder])?))?;
        let raw = self.raw_message(id)?;
        let mailbox = self.folder_mailboxes()?.remove(&folder).unwrap_or_default();
        let (s, subj, text, facts) = Library::triage_facts(&raw, &mailbox);
        let decision = decide_full(&s, if subj.is_empty() { &subject } else { &subj }, &text, Some(&facts), &ctx);
        Ok(Proposal { id, date, from, subject, folder, destination: destination(&decision.action), decision })
    }

    /// The mailbox a message is in: its account's address, lowercase ("" when it has none).
    pub fn message_mailbox(&self, id: i64) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT lower(COALESCE(a.address, '')) FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account WHERE l.message = ?1 ORDER BY l.id LIMIT 1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Bulk senders with a mailbox of their own (sender key → folder).
    pub fn bulk_folders(&self) -> Result<HashMap<String, String>> {
        Ok(self.setting(BULK_FOLDERS_SETTING)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    pub fn set_bulk_folders(&self, folders: &HashMap<String, String>) -> Result<()> {
        self.set_setting(BULK_FOLDERS_SETTING, &serde_json::to_string(folders)?)
    }

    /// Reads a message enough to decide: (signals, subject, the start of its text).
    fn triage_input(raw: &[u8]) -> (Signals, String, String) {
        let (s, subject, text, _) = Library::triage_facts(raw, "");
        (s, subject, text)
    }

    /// Reads a message for deciding: its signals, subject, the start of its text, and the facts
    /// exceptions and inclusions test (every header, recipients, the whole text).
    fn triage_facts(raw: &[u8], mailbox: &str) -> (Signals, String, String, Facts) {
        let facts = Facts::of(raw, mailbox);
        let s = signals(&facts.headers);
        let subject = facts.headers.iter().find(|(n, _)| n == "subject").map(|(_, v)| v.clone()).unwrap_or_default();
        let text: String = facts.body.chars().take(2000).collect();
        (s, subject, text, facts)
    }

    /// The dry run: decides for the messages in `folders` dated `since` or later (newest
    /// first, at most `limit`), moving nothing.
    pub fn triage_preview(&self, folders: &[i64], since: i64, limit: usize, progress: &dyn crate::Progress) -> Result<Preview> {
        let ctx = self.triage_context_for(Some(&self.mailboxes_of_folders(folders)?))?;
        let marks = vec!["?"; folders.len().max(1)].join(",");
        let sql = format!(
            "SELECT l.message, l.folder, m.date, m.from_display, m.subject, m.pack, m.pack_offset, m.pack_len FROM locations l JOIN messages m ON m.id = l.message
             WHERE l.folder IN ({marks}) AND m.date >= ? ORDER BY m.date DESC LIMIT ?"
        );
        let mut values: Vec<i64> = if folders.is_empty() { vec![-1] } else { folders.to_vec() };
        values.extend([since, limit as i64]);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<Row> = stmt
            .query_map(rusqlite::params_from_iter(values), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, Library::location(r, 5)?)))?
            .collect::<rusqlite::Result<_>>()?;
        self.decide_rows(rows, &ctx, progress)
    }

    /// The dry run on chosen messages: (message, the folder it's in), e.g. a selection in a
    /// mailbox or in search results (each from its own folder).
    pub fn triage_messages(&self, items: &[(i64, i64)], progress: &dyn crate::Progress) -> Result<Preview> {
        let folders: Vec<i64> = items.iter().map(|(_, f)| *f).collect();
        let ctx = self.triage_context_for(Some(&self.mailboxes_of_folders(&folders)?))?;
        let mut stmt = self.conn.prepare("SELECT date, from_display, subject, pack, pack_offset, pack_len FROM messages WHERE id = ?1")?;
        let mut rows: Vec<Row> = Vec::with_capacity(items.len());
        for (id, folder) in items {
            let row = stmt.query_row([id], |r| Ok((*id, *folder, r.get(0)?, r.get(1)?, r.get(2)?, Library::location(r, 3)?)));
            match row {
                Ok(row) => rows.push(row),
                Err(rusqlite::Error::QueryReturnedNoRows) => {}
                Err(e) => return Err(e.into()),
            }
        }
        // newest first: the
        // proposals, and the moves made from them, come in that order
        rows.sort_by(|a, b| b.2.cmp(&a.2).then(b.0.cmp(&a.0)));
        self.decide_rows(rows, &ctx, progress)
    }

    fn decide_rows(&self, rows: Vec<Row>, ctx: &Context, progress: &dyn crate::Progress) -> Result<Preview> {
        let total = rows.len() as u64;
        let mut preview = Preview { looked_at: total, ..Default::default() };
        let done = std::sync::atomic::AtomicU64::new(0);
        let mailboxes = self.folder_mailboxes()?;
        for chunk in rows.chunks(500) {
            if progress.canceled() {
                return Err(Error::Canceled);
            }
            let packs = &self.packs;
            let decided: Vec<Proposal> = chunk
                .par_iter()
                .filter_map(|(id, folder, date, from, subject, location)| {
                    let raw = packs.read(*location).ok()?.raw;
                    let (s, subj, text, facts) = Library::triage_facts(&raw, mailboxes.get(folder).map_or("", String::as_str));
                    let decision = decide_full(&s, if subj.is_empty() { subject } else { &subj }, &text, Some(&facts), ctx);
                    done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Some(Proposal { id: *id, date: *date, from: from.clone(), subject: subject.clone(), folder: *folder, destination: destination(&decision.action), decision })
                })
                .collect();
            progress.report(done.load(std::sync::atomic::Ordering::Relaxed), total, &format!("Deciding: message {} of {}", done.load(std::sync::atomic::Ordering::Relaxed), total));
            preview.proposals.extend(decided);
        }
        give_bulk_senders_folders(&mut preview.proposals, &ctx.bulk_folders);
        for p in &preview.proposals {
            *preview.counts.entry(p.decision.category.name().to_string()).or_default() += 1;
        }
        Ok(preview)
    }

    /// Carries out chosen proposals: (message, the folder it's in, its decision). Moves happen in
    /// the library and are queued for the server (`move_messages`); every decision is recorded.
    /// Returns how many moved.
    pub fn triage_apply(&self, chosen: &[Chosen]) -> Result<u64> {
        let mut moved = 0;
        // bulk senders given a mailbox of their own: their later mail goes there too
        let mut folders = self.bulk_folders()?;
        let before = folders.len();
        for d in chosen {
            if let (Some(key), Some(Action::Move { folder })) = (&d.sender_key, &d.action) {
                if folder.starts_with(&format!("{MARKETING_FOLDER}/")) || folder.starts_with(&format!("{NOTIFICATIONS_FOLDER}/")) {
                    folders.entry(key.clone()).or_insert_with(|| folder.clone());
                }
            }
        }
        if folders.len() != before {
            self.set_bulk_folders(&folders)?;
        }
        // In batches of 500: the moves of a batch go folder pair by folder pair (one transaction
        // each), its records in one more, and the write lock is let go between batches so the
        // download keeps storing new mail. One message at a time (a commit each) took 8 hours
        // over a 216,000-message Inbox and kept the download from storing anything meanwhile.
        let mut accounts: HashMap<i64, i64> = HashMap::new();
        let mut targets: HashMap<(i64, String), i64> = HashMap::new();
        for batch in chosen.chunks(500) {
            let mut groups: std::collections::BTreeMap<(i64, i64), Vec<i64>> = Default::default();
            let mut went: HashMap<i64, i64> = HashMap::new();
            for c in batch {
                let account = match accounts.get(&c.folder) {
                    Some(a) => *a,
                    None => {
                        let a: i64 = self.conn.query_row("SELECT account FROM folders WHERE id = ?1", [c.folder], |r| r.get(0))?;
                        accounts.insert(c.folder, a);
                        a
                    }
                };
                let key = match &c.action {
                    None | Some(Action::Label { .. }) => None,
                    Some(Action::Trash) => Some("\u{0}trash".to_string()),
                    Some(Action::Spam) => Some("\u{0}spam".to_string()),
                    Some(Action::Move { folder }) => Some(folder.clone()),
                };
                let Some(key) = key else { continue };
                let to = match targets.get(&(account, key.clone())) {
                    Some(to) => *to,
                    None => {
                        let to = match &c.action {
                            Some(Action::Trash) => self.trash_folder(account)?,
                            Some(Action::Spam) => self.spam_folder(account)?,
                            Some(Action::Move { folder }) => crate::db::ensure_folder(&self.conn, account, folder)?,
                            _ => unreachable!(),
                        };
                        targets.insert((account, key), to);
                        to
                    }
                };
                if to != c.folder {
                    groups.entry((c.folder, to)).or_default().push(c.id);
                    went.insert(c.id, to);
                }
            }
            for ((from, to), ids) in groups {
                moved += self.move_messages(&ids, from, to)?;
            }
            // (what was done to each, why, for Activity's details: under the job that ran the
            // spam filter, else the user's own Sort with the Spam Filter)
            let job = crate::job_log::current().unwrap_or("sort");
            let mut said: Vec<(i64, String)> = Vec::with_capacity(batch.len());
            for c in batch {
                let why: Vec<&str> = c.reasons.iter().take(3).map(String::as_str).collect();
                let why = if why.is_empty() { String::new() } else { format!(" ({})", why.join("; ")) };
                let done = match went.get(&c.id) {
                    Some(to) => format!("Spam filter: {} → moved to {}{why}", c.category, self.folder_path_of(*to)),
                    None => format!("Spam filter: {}, stays{}{why}", c.category, if c.maybe { " (listed in Maybe Spam)" } else { "" }),
                };
                said.push((c.id, done));
            }
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            let result = (|| -> Result<()> {
                let mut stmt = self.conn.prepare_cached(
                    "INSERT INTO triage (message, category, priority, reasons, decided, applied) VALUES (?1, ?2, ?3, ?4, ?5, 1)
                     ON CONFLICT (message) DO UPDATE SET category = excluded.category, priority = excluded.priority, reasons = excluded.reasons, decided = excluded.decided, applied = 1",
                )?;
                for c in batch {
                    stmt.execute(params![c.id, c.category, c.priority as i64, serde_json::to_string(&c.reasons)?, now()])?;
                    self.set_maybe(c.id, c.maybe)?;
                }
                // (in the same save as the decisions: one commit a batch)
                for (id, done) in &said {
                    self.log_job(job, Some(*id), done)?;
                }
                Ok(())
            })();
            self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
            result?;
        }
        Ok(moved)
    }

    /// Sorts messages now (what the "sort" rule action and Sort Selected do): decides and moves.
    /// Returns (moved, how many per category).
    pub fn triage_now(&self, items: &[(i64, i64)]) -> Result<(u64, Vec<(String, u64)>)> {
        let preview = self.triage_messages(items, &crate::progress::Silent)?;
        let chosen: Vec<Chosen> = preview.proposals.iter().map(Chosen::from).collect();
        let moved = self.triage_apply(&chosen)?;
        let mut counts: Vec<(String, u64)> = preview.counts.into_iter().collect();
        counts.sort();
        Ok((moved, counts))
    }

    /// Lists a message in the Maybe Spam view, or takes it out.
    pub fn set_maybe(&self, id: i64, maybe: bool) -> Result<()> {
        if maybe {
            self.conn.execute("INSERT OR IGNORE INTO tags (tag, date, message) SELECT ?1, date, id FROM messages WHERE id = ?2", params![MAYBE_VIEW, id])?;
        } else {
            self.conn.execute("DELETE FROM tags WHERE tag = ?1 AND message = ?2", params![MAYBE_VIEW, id])?;
        }
        Ok(())
    }

    /// The account's "Spam" folder, where Eudora puts spam (made if none).
    pub fn spam_folder(&self, account: i64) -> Result<i64> {
        crate::db::ensure_folder(&self.conn, account, SPAM_FOLDER)
    }

    /// The account's Junk folder (Exchange's "Junk Email", else "Junk" / "Spam"), made if none.
    pub fn junk_folder(&self, account: i64) -> Result<i64> {
        for name in ["Junk Email", "Junk", "Spam", "[Gmail]/Spam"] {
            if let Some(id) = self.conn.query_row("SELECT id FROM folders WHERE account = ?1 AND lower(path) = lower(?2)", params![account, name], |r| r.get(0)).ok() {
                return Ok(id);
            }
        }
        crate::db::ensure_folder(&self.conn, account, "Junk Email")
    }

    /// Messages like `id` (its fingerprint's rule keys) in the last `days` days: (the keys,
    /// how many, the newest few) — the preview before a "messages like this one" rule is saved.
    pub fn messages_like(&self, id: i64, days: i64, sample: usize) -> Result<(Fingerprint, u64, Vec<(i64, i64, String, String)>)> {
        let raw = self.raw_message(id)?;
        let (s, subject, _) = Library::triage_input(&raw);
        let keys = Fingerprint::of(&s, &subject).rule_keys();
        let since = now() - days * 86400;
        let mut stmt = self.conn.prepare("SELECT id, date, from_display, subject, pack, pack_offset, pack_len FROM messages WHERE date >= ?1 ORDER BY date DESC")?;
        let rows: Vec<(i64, i64, String, String, crate::pack::Location)> =
            stmt.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, Library::location(r, 4)?)))?.collect::<rusqlite::Result<_>>()?;
        let packs = &self.packs;
        let mut hits: Vec<(i64, i64, String, String)> = rows
            .par_iter()
            .filter_map(|(id, date, from, subject, location)| {
                let raw = packs.read(*location).ok()?.raw;
                let headers = crate::header_index::read_headers(&raw);
                let s = signals(&headers);
                keys.matches(&Fingerprint::of(&s, subject)).then(|| (*id, *date, from.clone(), subject.clone()))
            })
            .collect();
        hits.sort_by(|a, b| b.1.cmp(&a.1));
        let count = hits.len() as u64;
        hits.truncate(sample);
        Ok((keys, count, hits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header_index::read_headers;

    fn ctx() -> Context {
        let mut lists = SenderLists::default();
        lists.allowed.insert("carol@customer.example".into());
        lists.blocked.push(("@spammy.example".into(), Action::Trash, 1));
        Context { lists, mailbox_lists: HashMap::new(), rules: Vec::new(), phrases: Phrases::new(&default_phrases()), bulk_folders: HashMap::new(), languages: vec!["en".into(), "it".into()], phrases_groups: default_phrases(), rule_settings: HashMap::new(), own_domains: vec!["corp.example".into()], bayes: HashMap::new(), domains: HashMap::new(), listings: HashMap::new() }
    }

    fn decide_raw(raw: &str, ctx: &Context) -> Decision {
        let raw = raw.as_bytes();
        let (s, subject, text) = Library::triage_input(raw);
        decide(&s, &subject, &text, ctx)
    }

    #[test]
    fn mail_is_sorted_with_reasons() {
        let c = ctx();
        let newsletter = "From: Deals <news@shop.example>\r\nList-Id: <deals.shop.example>\r\nList-Unsubscribe: <https://shop.example/u>\r\nSubject: 20% off\r\n\r\nbuy now";
        let d = decide_raw(newsletter, &c);
        assert_eq!(d.category, Category::List);
        assert!(d.symbols.iter().any(|s| s.why.contains("deals.shop.example")));
        let bulk = "From: no-reply@app.example\r\nX-SG-EID: x\r\nList-Unsubscribe: <https://app.example/u>\r\nSubject: Your weekly report\r\n\r\nstats";
        assert_eq!(decide_raw(bulk, &c).category, Category::Bulk);
        let spam = "From: winner@lotto.example\r\nX-Forefront-Antispam-Report: SFV:SPM;SCL:9\r\nAuthentication-Results: x; dmarc=fail\r\nSubject: you won\r\n\r\nclaim";
        let d = decide_raw(spam, &c);
        assert_eq!((d.category, d.action.clone()), (Category::Spam, Some(Action::Spam)));
        let friend = "From: Carol <carol@customer.example>\r\nSubject: lunch?\r\n\r\nfree tomorrow?";
        assert_eq!(decide_raw(friend, &c).category, Category::People);
        let blocked = "From: a@mail.spammy.example\r\nSubject: hi\r\n\r\nhi";
        let d = decide_raw(blocked, &c);
        assert_eq!((d.category, d.action), (Category::Blocked, Some(Action::Trash)));
        // mass mail from an automatically whitelisted sender is still bulk; from one the user
        // added, it stays
        let zoom = "From: Zoom <no-reply@zoom.us>
List-Unsubscribe: <https://zoom.us/u>
Subject: Webinar

join";
        let mut c2 = ctx();
        c2.lists.allowed.insert("no-reply@zoom.us".into());
        assert_eq!(decide_raw(zoom, &c2).category, Category::Bulk);
        c2.lists.chosen.push("no-reply@zoom.us".into());
        assert_eq!(decide_raw(zoom, &c2).category, Category::People);
        let stranger = "From: someone@new.example\r\nSubject: question about your service\r\n\r\nhello";
        assert_eq!(decide_raw(stranger, &c).category, Category::Other);
    }

    #[test]
    fn every_rule_can_be_switched_off_or_given_exceptions_and_inclusions() {
        let test = |field: &str, value: &str| Test { field: field.into(), verb: crate::rules::Verb::Contains, value: value.into(), unless: false };
        let run = |raw: &str, c: &Context| {
            let (s, subject, text, facts) = Library::triage_facts(raw.as_bytes(), "");
            decide_full(&s, &subject, &text, Some(&facts), c)
        };
        let bank = "From: Bank <alerts@mybank.example>\r\nList-Unsubscribe: <https://mybank.example/u>\r\nSubject: Our new rates\r\n\r\nx";
        let mut c = ctx();
        let d = run(bank, &c);
        assert_eq!(d.category, Category::Bulk);
        assert_eq!(d.symbols[0].rule, "bulk", "the rule that decided comes first: {d:?}");
        // an exception: Bulk mail, except from my bank
        c.rule_settings.insert("bulk".into(), RuleSettings { exceptions: vec![test("From:", "@mybank.example")], ..Default::default() });
        assert_eq!(run(bank, &c).category, Category::Other);
        // switched off: no Bulk at all
        c.rule_settings.insert("bulk".into(), RuleSettings { off: true, ..Default::default() });
        assert_eq!(run(bank, &c).category, Category::Other);
        // an inclusion: a personal-looking message that's really a mass mailing
        let mut c = ctx();
        let webinar = "From: Sam <sam@vendor.example>\r\nSubject: Join our webinar\r\n\r\nx";
        assert_eq!(run(webinar, &c).category, Category::Other);
        c.rule_settings.insert("bulk".into(), RuleSettings { inclusions: vec![test("Subject:", "webinar")], ..Default::default() });
        let d = run(webinar, &c);
        assert_eq!(d.category, Category::Bulk);
        assert!(d.symbols[0].why.contains("inclusion"), "{d:?}");
        // a blacklist entry with an exception, and a priority group switched off
        let mut c = ctx();
        let blocked = "From: a@mail.spammy.example\r\nSubject: hi\r\n\r\nhi";
        assert_eq!(run(blocked, &c).category, Category::Blocked);
        c.rule_settings.insert("block:@spammy.example".into(), RuleSettings { exceptions: vec![test("Subject:", "hi")], ..Default::default() });
        assert_eq!(run(blocked, &c).category, Category::Other);
        let tax = "From: Carol <carol@customer.example>\r\nSubject: taxes due\r\n\r\nx";
        assert_eq!(run(tax, &c).category, Category::Priority);
        c.rule_settings.insert("phrases:Taxes".into(), RuleSettings { off: true, ..Default::default() });
        assert_eq!(run(tax, &c).category, Category::People);
    }

    #[test]
    fn a_domain_registered_days_ago_counts_toward_spam() {
        let mut c = ctx();
        let raw = "From: Support <help@new-bank-login.example>\r\nSubject: Verify your account\r\n\r\nhi";
        let base = decide_raw(raw, &c);
        c.domains.insert("new-bank-login.example".into(), crate::reputation::DomainFacts { registered: Some(crate::library::now() - 3 * 86_400), first_seen: None });
        let young = decide_raw(raw, &c);
        assert!(young.symbols.iter().any(|s| s.rule == "young-domain" && s.score == 3.5), "{young:?}");
        assert!(young.spam_score > base.spam_score);
        // from someone the user writes to: never
        c.lists.allowed.insert("help@new-bank-login.example".into());
        assert!(!decide_raw(raw, &c).symbols.iter().any(|s| s.rule == "young-domain"));
    }

    #[test]
    fn a_rule_can_apply_in_some_mailboxes_only() {
        let run = |raw: &str, mailbox: &str, c: &Context| {
            let (s, subject, text, facts) = Library::triage_facts(raw.as_bytes(), mailbox);
            decide_full(&s, &subject, &text, Some(&facts), c)
        };
        let blocked = "From: a@mail.spammy.example\r\nSubject: hi\r\n\r\nhi";
        let mut c = ctx();
        c.rule_settings.insert("block:@spammy.example".into(), RuleSettings { only_in: vec!["noc@corp.example".into()], ..Default::default() });
        assert_eq!(run(blocked, "NOC@corp.example", &c).category, Category::Blocked, "in its mailbox (any case)");
        assert_eq!(run(blocked, "pat@corp.example", &c).category, Category::Other, "not in another");
        assert_eq!(run(blocked, "", &c).category, Category::Blocked, "a mailbox not known: everywhere");
    }

    #[test]
    fn lists_and_like_rules_of_one_mailbox_stay_there() {
        let run = |raw: &str, mailbox: &str, c: &Context| {
            let (s, subject, text, facts) = Library::triage_facts(raw.as_bytes(), mailbox);
            decide_full(&s, &subject, &text, Some(&facts), c)
        };
        let mut c = ctx();
        // noc@ has its own blacklist entry; pat@ doesn't
        let mut noc = c.lists.clone();
        noc.blocked.push(("@pest.example".into(), Action::Spam, 2));
        c.mailbox_lists.insert("noc@corp.example".into(), noc);
        let pest = "From: a@pest.example\r\nSubject: hi\r\n\r\nhi";
        assert_eq!(run(pest, "NOC@corp.example", &c).category, Category::Blocked);
        assert_eq!(run(pest, "pat@corp.example", &c).category, Category::Other);
        assert_eq!(run("From: a@mail.spammy.example\r\nSubject: hi\r\n\r\nhi", "noc@corp.example", &c).category, Category::Blocked, "every mailbox's entries too");
        // a "like this" rule in pat@ only
        let keys = Fingerprint { from_org: Some("offers.example".into()), ..Default::default() };
        c.rules.push(LikeRule { id: 1, name: "offers".into(), keys, action: Action::Spam, created: 0, enabled: true, mailbox: "pat@corp.example".into() });
        let offer = "From: deals@offers.example\r\nSubject: hi\r\n\r\nhi";
        assert_eq!(run(offer, "pat@corp.example", &c).category, Category::Rule);
        assert_ne!(run(offer, "noc@corp.example", &c).category, Category::Rule);
    }

    #[test]
    fn maybe_is_unsure_mail_that_stays() {
        let d = |category, score, action: Option<Action>| Decision { category, priority: false, action, spam_score: score, symbols: Vec::new(), sender_key: None, sender_name: None };
        assert!(d(Category::Other, 3.0, None).is_maybe());
        assert!(!d(Category::Other, 1.0, None).is_maybe(), "not unsure");
        assert!(!d(Category::People, 3.0, None).is_maybe(), "people the user writes to");
        assert!(!d(Category::Spam, 6.0, Some(Action::Spam)).is_maybe(), "spam moves");
    }

    #[test]
    fn the_filter_works_in_the_users_own_mailboxes_until_chosen_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::Silent).unwrap();
        let own = crate::db::graph_account(&library.conn, "graph", "Me@x.com", "Me", 0).unwrap();
        let shared = crate::db::graph_account(&library.conn, "graph-shared", "noc@x.com", "NOC", 0).unwrap();
        for a in [own, shared] {
            crate::db::ensure_folder(&library.conn, a, "Inbox").unwrap();
        }
        let on = |l: &Library| l.spam_filter_mailboxes().unwrap().into_iter().filter(|m| m.on).map(|m| m.address).collect::<Vec<_>>();
        assert_eq!(on(&library), vec!["me@x.com"], "shared mailboxes are other people's mail: off by default");
        assert_eq!(library.spam_filter_inboxes().unwrap().len(), 1);
        library.set_spam_filter_mailboxes(&["NOC@x.com".into()]).unwrap();
        assert_eq!(on(&library), vec!["noc@x.com"]);
        assert_eq!(library.spam_filter_accounts().unwrap().into_iter().collect::<Vec<_>>(), vec![shared]);
    }

    #[test]
    fn other_languages_go_to_their_own_junk_folder() {
        let c = ctx();
        let arabic = "From: Samir <samir@shop.example>\r\nSubject: \u{0639}\u{0631}\u{0636} \u{062e}\u{0627}\u{0635}\r\n\r\n\u{0645}\u{0631}\u{062d}\u{0628}\u{0627} \u{0628}\u{0643}\u{0645} \u{0641}\u{064a} \u{0645}\u{062a}\u{062c}\u{0631}\u{0646}\u{0627} \u{0627}\u{0644}\u{062c}\u{062f}\u{064a}\u{062f} \u{0627}\u{0644}\u{064a}\u{0648}\u{0645}";
        let d = decide_raw(arabic, &c);
        assert_eq!(d.action, Some(Action::Move { folder: OTHER_LANGUAGES_FOLDER.into() }), "{d:?}");
        assert!(d.symbols[0].why.contains("Arabic"));
        // a person the user writes to is left alone; English and Italian are fine
        let mut c2 = ctx();
        c2.lists.allowed.insert("samir@shop.example".into());
        assert_ne!(decide_raw(arabic, &c2).category, Category::Spam);
        let italian = "From: a@b.example\r\nSubject: Ciao\r\n\r\nCi vediamo domani alle dieci per il caffè, va bene?";
        assert_eq!(decide_raw(italian, &c).category, Category::Other);
        // a "mailing list" in Arabic with a list's English footer: by its subject, or by
        // its body
        let footer = "\r\n--\r\nYou received this message because you are subscribed to the Google Groups \"examplegroup123\" group.\r\nTo unsubscribe from this group and stop receiving emails from it, send an email to examplegroup123+unsubscribe@googlegroups.com.\r\nTo view this discussion visit https://groups.google.com/d/msgid/examplegroup123/abc.";
        let list = "From: samar <samar@gmail.example>\r\nList-Id: <examplegroup123.googlegroups.com>\r\nPrecedence: list\r\nList-Unsubscribe: <mailto:examplegroup123+unsubscribe@googlegroups.com>\r\n";
        let by_subject = format!("{list}Subject: \u{0633}\u{062c}\u{0644} \u{0627}\u{0644}\u{0627}\u{0646} / \u{0634}\u{0647}\u{0627}\u{062f}\u{0629} \u{0645}\u{062f}\u{064a}\u{0631} \u{0627}\u{0644}\u{062c}\u{0648}\u{062f}\u{0629} (AI, BI, KPI, QMS & RPA)\r\n\r\nCAIPP 2026{footer}");
        assert_eq!(decide_raw(&by_subject, &c).action, Some(Action::Move { folder: OTHER_LANGUAGES_FOLDER.into() }));
        let by_body = format!("{list}Subject: CAIPP 2026 AI\r\n\r\n\u{0645}\u{0631}\u{062d}\u{0628}\u{0627} \u{0628}\u{0643}\u{0645} \u{0641}\u{064a} \u{0645}\u{062a}\u{062c}\u{0631}\u{0646}\u{0627} \u{0627}\u{0644}\u{062c}\u{062f}\u{064a}\u{062f}{footer}");
        assert_eq!(decide_raw(&by_body, &c).action, Some(Action::Move { folder: OTHER_LANGUAGES_FOLDER.into() }));
        assert_eq!(foreign_writing("Привет, как дела? Давно не виделись, друг мой", &["en".into()]), Some("Cyrillic (Russian, Ukrainian…)"));
        assert_eq!(foreign_writing("今日は良い天気ですね、明日も晴れるといいな", &["ja".into()]), None);
        // the same alphabet: told by the little words
        let mine: Vec<String> = vec!["en".into(), "it".into()];
        let french = "Bonjour à tous, nous vous invitons à la conférence des mosquées du monde et à la Omra de novembre. Les places sont limitées : réservez votre voyage avec nous dès maintenant pour être sûr de partir.";
        assert_eq!(foreign_writing(french, &mine), Some("French"));
        assert_eq!(foreign_subject("Conférence des mosquées du monde et Omra de novembre", &mine), Some("French"));
        let english = "Hi Pat, thanks for the update on the Paris office. We will have the new router there by Friday and the team can finish the install on Monday if that works for you.";
        assert_eq!(foreign_writing(english, &mine), None);
        let italian = "Ciao Pato, grazie per il messaggio. Ci vediamo la settimana prossima a Roma per la riunione con il cliente, come abbiamo detto anche al telefono.";
        assert_eq!(foreign_writing(italian, &mine), None);
        let both = format!("{english} {french}");
        assert_eq!(foreign_writing(&both, &mine), None, "bilingual mail stays");
        assert_eq!(foreign_subject("Re: Meeting at Café de la Paix", &mine), None);
    }

    #[test]
    fn big_bulk_senders_get_a_mailbox_of_their_own() {
        let c = ctx();
        let proposal = |id: i64, raw: String| {
            let (s, subject, text) = Library::triage_input(raw.as_bytes());
            let decision = decide(&s, &subject, &text, &c);
            Proposal { id, date: 0, from: s.from.clone(), subject, folder: 1, destination: destination(&decision.action), decision }
        };
        let mut proposals: Vec<Proposal> = (0..150)
            .map(|i| {
                let addr = if i % 2 == 0 { "deals@us.travelzoo.com" } else { "top20@e.travelzoo.com" };
                proposal(i, format!("From: Travelzoo <{addr}>\r\nAuthentication-Results: mx; dmarc=pass
List-Unsubscribe: <https://travelzoo.com/u>\r\nSubject: Deal {i}\r\n\r\nx"))
            })
            .collect();
        proposals.push(proposal(999, "From: Small Shop <news@small.example>\r\nList-Unsubscribe: <https://small.example/u>\r\nSubject: Hi\r\n\r\nx".into()));
        give_bulk_senders_folders(&mut proposals, &HashMap::new());
        assert_eq!(proposals[0].destination.as_deref(), Some("Bulk/Marketing/Travelzoo"));
        assert_eq!(proposals[1].destination.as_deref(), Some("Bulk/Marketing/Travelzoo"), "every address of the company");
        assert_eq!(proposals[150].destination.as_deref(), Some(MARKETING_FOLDER), "a small sender stays in Bulk/Marketing");
        // once made, later mail goes there by itself
        let mut c2 = ctx();
        c2.bulk_folders.insert("travelzoo.com".into(), "Bulk/Travelzoo".into()); // (remembered before the split: still used)
        let (s, subject, text) = Library::triage_input(b"From: Travelzoo <x@news.travelzoo.com>\r\nAuthentication-Results: mx; dmarc=pass\r\nList-Unsubscribe: <https://t/u>\r\nSubject: y\r\n\r\nz");
        assert_eq!(decide(&s, &subject, &text, &c2).action, Some(Action::Move { folder: "Bulk/Travelzoo".into() }));
    }

    #[test]
    fn systems_are_notifications_but_a_colleague_saying_down_stays() {
        let mut c = ctx();
        // the company's own systems: each by its own address, named after it
        let backup = "From: VeeamCorp <veeamcorp@hq.corp.example>\r\nSubject: [Success] APP1 - Job APP1\r\n\r\nok";
        let d = decide_raw(backup, &c);
        assert_eq!(d.category, Category::Notification, "{d:?}");
        assert_eq!(d.sender_key.as_deref(), Some("notifications:veeamcorp@hq.corp.example"), "own company: by address");
        let ping = "From: monitor@hq.corp.example\r\nSubject: [Monitor Ping] Host srv7 is up\r\n\r\nx";
        assert_eq!(decide_raw(ping, &c).category, Category::Notification);
        // a colleague (whitelisted) writing "is down" is a person: stays in the Inbox
        c.lists.allowed.insert("dylan@corp.example".into());
        let colleague = "From: Dylan <dylan@corp.example>\r\nSubject: The main switch is down\r\n\r\nLooking at it now.";
        assert_eq!(decide_raw(colleague, &c).category, Category::People);
        // root@ and postmaster@ are systems
        assert_eq!(decide_raw("From: root@web2.example\r\nSubject: lfd: blocked 1.2.3.4\r\n\r\nx", &c).category, Category::Notification);
        // a marketing address without an unsubscribe link is still marketing
        assert_eq!(decide_raw("From: marketing@e-email.shop.example\r\nSubject: Inside the vault\r\n\r\nx", &c).category, Category::Bulk);
    }

    #[test]
    fn fake_well_known_senders_are_caught_and_kept_apart() {
        let c = ctx();
        // claims Amazon, fails DMARC: phishing points, and never Amazon's folder or kind
        let fake = "From: \"Amazon.com\" <order-update@amazon.com>\r\nAuthentication-Results: mx; spf=fail; dkim=none; dmarc=fail\r\nSubject: Your order has shipped\r\n\r\nclick";
        let d = decide_raw(fake, &c);
        assert!(d.symbols.iter().any(|s| s.rule == "forged-known" && s.why.contains("Amazon")), "{d:?}");
        assert!(!d.symbols.iter().any(|s| s.rule == "known-senders"), "an unproven claim isn't a known sender");
        // no results at all (old imported mail): not phishing, but not trusted as Amazon either
        let bare = "From: order-update@amazon.com\r\nX-SES-Outgoing: x\r\nSubject: hello\r\n\r\nx";
        let d = decide_raw(bare, &c);
        assert!(!d.symbols.iter().any(|s| s.rule == "forged-known" || s.rule == "known-senders"), "{d:?}");
        assert_eq!(d.sender_key.as_deref(), Some("notifications:order-update@amazon.com"), "its own address, not amazon.com's folder");
        // the real one: known, a notification, Amazon's
        let real = "From: order-update@amazon.com\r\nAuthentication-Results: mx; dkim=pass header.d=amazon.com; dmarc=pass\r\nSubject: hello\r\n\r\nx";
        let d = decide_raw(real, &c);
        assert!(d.symbols.iter().any(|s| s.rule == "known-senders"), "{d:?}");
        assert_eq!((d.category, d.sender_key.as_deref()), (Category::Notification, Some("notifications:amazon.com")));
        // lookalike domains are no one we know
        assert!(crate::triage_known::known("order-update@amazon.com.evil.example").is_none());
        assert!(crate::triage_known::known("order-update@notamazon.com").is_none());
    }

    #[test]
    fn notifications_are_apart_from_marketing_each_sender_in_its_own_folder() {
        let c = ctx();
        let proposal = |id: i64, raw: String| {
            let (s, subject, text) = Library::triage_input(raw.as_bytes());
            let decision = decide(&s, &subject, &text, &c);
            Proposal { id, date: 0, from: s.from.clone(), subject, folder: 1, destination: destination(&decision.action), decision }
        };
        // Amazon order updates (through Amazon SES, no unsubscribe link): notifications
        let mut proposals: Vec<Proposal> = (0..12)
            .map(|i| proposal(i, format!("From: \"Amazon.com\" <order-update@amazon.com>\r\nAuthentication-Results: mx; dkim=pass header.d=amazon.com; dmarc=pass\r\nDKIM-Signature: d=amazon.com; s=x\r\nX-SES-Outgoing: x\r\nSubject: Your order {i} has shipped\r\n\r\nx")))
            .collect();
        // DocuSign: a different person's name each time, one company
        for i in 0..12 {
            proposals.push(proposal(100 + i, format!("From: \"Person {i} via Docusign\" <dse@docusign.net>\r\nAuthentication-Results: mx; dmarc=pass\r\nAuto-Submitted: auto-generated\r\nSubject: Please sign\r\n\r\nx")));
        }
        // a rare one, and a marketing message for contrast
        proposals.push(proposal(200, "From: alerts@rarebank.example\r\nAuto-Submitted: auto-generated\r\nSubject: Alert\r\n\r\nx".into()));
        proposals.push(proposal(300, "From: Deals <deals@shop.example>\r\nList-Unsubscribe: <https://shop.example/u>\r\nSubject: Sale\r\n\r\nx".into()));
        assert_eq!(proposals[0].decision.category, Category::Notification);
        assert_eq!(proposals[0].decision.symbols[0].rule, "known-senders", "a verified well-known sender decided it");
        assert_eq!(proposals[25].decision.category, Category::Bulk, "an unsubscribe link = marketing");
        give_bulk_senders_folders(&mut proposals, &HashMap::new());
        assert_eq!(proposals[0].destination.as_deref(), Some("Notifications/Amazon"));
        assert_eq!(proposals[12].destination.as_deref(), Some("Notifications/DocuSign"), "a well-known sender: its own name");
        assert_eq!(proposals[24].destination.as_deref(), Some(NOTIFICATIONS_FOLDER), "a rare sender shares the folder");
        assert_eq!(proposals[25].destination.as_deref(), Some(MARKETING_FOLDER));
    }

    #[test]
    fn security_notices_stay_in_the_inbox() {
        let c = ctx();
        let cisa = "Authentication-Results: mx; spf=pass; dkim=pass header.d=cisa.gov; dmarc=pass header.from=cisa.gov\r\nFrom: CISA <cisa@messages.cisa.gov>\r\nList-Id: <cisa.govdelivery>\r\nList-Unsubscribe: <https://x/u>\r\nPrecedence: bulk\r\nSubject: CISA Releases Seven Industrial Control Systems Advisories\r\n\r\nx";
        let d = decide_raw(cisa, &c);
        assert!(d.category == Category::Priority && d.action.is_none() && d.symbols[0].rule == "security-notice", "{d:?}");
        let fwd = "From: Sam <sam@corp.example>\r\nSubject: FW: Microsoft security update for Exchange Server\r\n\r\nplease patch";
        let d = decide_raw(fwd, &c);
        assert_eq!(d.category, Category::Priority, "{d:?}");
        let soc = "Authentication-Results: mx; dmarc=pass header.from=soc.example\r\nFrom: SOC <alerts@soc.example>\r\nAuto-Submitted: auto-generated\r\nSubject: Security alert: Malicious Binary found\r\n\r\nx";
        assert_ne!(decide_raw(soc, &c).category, Category::Priority, "a SOC service's detections are notifications");
    }

    #[test]
    fn priority_needs_a_believable_sender() {
        let c = ctx();
        // a utility's real notice (signed) and an upset customer: priority
        let utility = "From: billing@power.example\r\nAuthentication-Results: x; spf=pass; dkim=pass; dmarc=pass\r\nList-Unsubscribe: <https://power.example/u>\r\nSubject: Disconnect notice for account 1234\r\n\r\npay by Friday";
        let d = decide_raw(utility, &c);
        assert!(d.priority && d.category == Category::Priority, "{d:?}");
        let upset = "From: Carol <carol@customer.example>\r\nSubject: Outage\r\n\r\nThis is unacceptable, third time this month.";
        assert_eq!(decide_raw(upset, &c).category, Category::Priority);
        // marketing copy isn't an upset customer; a failing sender saying "final notice" is suspicious
        let promo = "From: deals@shop.example\r\nList-Id: <deals.shop.example>\r\nSubject: Don't miss out\r\n\r\nPrices this low are unacceptable!";
        assert_eq!(decide_raw(promo, &c).category, Category::List);
        let phish = "From: support@bank.example\r\nAuthentication-Results: x; spf=fail; dkim=fail; dmarc=fail\r\nSubject: FINAL NOTICE: account suspended\r\n\r\nclick here";
        let d = decide_raw(phish, &c);
        assert!(!d.priority && d.symbols.iter().any(|s| s.name == "SUSPICIOUS_NOTICE"), "{d:?}");
    }

    #[test]
    fn like_this_rules_survive_rotating_addresses() {
        let a = signals(&read_headers(b"From: deals-41@e.brand.example\r\nReturn-Path: <b-1@bounce.brand.example>\r\nDKIM-Signature: d=brand.example; s=x\r\nAuthentication-Results: x; dkim=pass\r\n\r\n"));
        let b = signals(&read_headers(b"From: offers-99@mail.brand.example\r\nReturn-Path: <b-7@bounce.brand.example>\r\nDKIM-Signature: d=brand.example; s=y\r\nAuthentication-Results: x; dkim=pass\r\n\r\n"));
        let keys = Fingerprint::of(&a, "Deal 1").rule_keys();
        assert_eq!(keys.dkim_org.as_deref(), Some("brand.example"));
        assert!(keys.matches(&Fingerprint::of(&b, "Deal 2")), "another address of the same sender");
        // never only a shared service domain: a gmail sender is keyed by address
        let g = signals(&read_headers(b"From: someone@gmail.com\r\n\r\n"));
        let keys = Fingerprint::of(&g, "hello").rule_keys();
        assert_eq!(keys.from_address.as_deref(), Some("someone@gmail.com"));
        assert!(!keys.matches(&Fingerprint::of(&signals(&read_headers(b"From: other@gmail.com\r\n\r\n")), "hello")));
        assert_eq!(subject_template("Your order #12,345 shipped on 10/9/2026"), "your order # shipped on #");
        assert_eq!(org_domain("mail.shop.co.uk"), "shop.co.uk");
    }
}
