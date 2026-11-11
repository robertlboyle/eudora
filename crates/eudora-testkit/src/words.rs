//! Text for synthetic mail: common English words for most of it, plus a long tail of made-up
//! words, so the index sees a realistic spread of frequent and rare terms.

use crate::rng::Rng;

pub const COMMON: &[&str] = &[
    "the", "of", "and", "to", "in", "is", "you", "that", "it", "he", "was", "for", "on", "are", "as", "with", "his",
    "they", "at", "be", "this", "have", "from", "or", "one", "had", "by", "word", "but", "not", "what", "all", "were",
    "we", "when", "your", "can", "said", "there", "use", "an", "each", "which", "she", "do", "how", "their", "if",
    "will", "up", "other", "about", "out", "many", "then", "them", "these", "so", "some", "her", "would", "make",
    "like", "him", "into", "time", "has", "look", "two", "more", "write", "go", "see", "number", "no", "way", "could",
    "people", "my", "than", "first", "water", "been", "call", "who", "oil", "its", "now", "find", "long", "down",
    "day", "did", "get", "come", "made", "may", "part", "meeting", "project", "report", "invoice", "schedule",
    "budget", "client", "contract", "review", "draft", "attached", "please", "thanks", "regards", "tomorrow",
    "today", "monday", "friday", "office", "server", "network", "account", "password", "update", "release",
    "version", "customer", "order", "shipping", "payment", "quarter", "sales", "team", "lunch", "dinner", "family",
    "weekend", "vacation", "flight", "hotel", "photos", "birthday", "party", "question", "answer", "problem",
    "solution", "proposal", "deadline", "minutes", "agenda", "notes", "summary", "details", "price", "quote",
    "estimate", "install", "support", "ticket", "issue", "fixed", "working", "office", "fiber", "internet",
    "router", "switch", "cable", "tower", "antenna", "signal", "outage", "maintenance", "window", "upgrade",
];

const SYLLABLES: &[&str] = &[
    "ka", "lo", "ri", "ven", "tor", "mi", "sa", "dun", "pel", "qua", "zor", "bel", "ith", "nar", "o", "us", "ex", "lim",
    "dra", "fen", "gol", "hap", "jun", "kel", "mor", "nim", "pax", "rul", "sev", "tam", "ulo", "vix", "wen", "yar",
    "zep", "bri", "cos", "dax", "eru", "fio", "gra", "hes", "ivo", "jas", "kru", "lan", "mex", "nok", "orb", "pim",
];

/// A made-up word from a number (the same number, the same word).
pub fn rare_word(n: u64) -> String {
    let s = SYLLABLES.len() as u64;
    let mut word = String::new();
    let parts = 2 + (n % 3);
    let mut x = n;
    for _ in 0..parts {
        word.push_str(SYLLABLES[(x % s) as usize]);
        x /= s;
        x = x.wrapping_mul(2_654_435_761).wrapping_add(n) % 1_000_003;
    }
    word
}

pub fn word(rng: &mut Rng) -> String {
    if rng.chance(0.72) { COMMON[rng.skewed(COMMON.len() as u64) as usize].to_string() } else { rare_word(rng.skewed(60_000)) }
}

pub fn sentence(rng: &mut Rng) -> String {
    let n = rng.range(5, 18);
    let mut s = String::new();
    for i in 0..n {
        let w = word(rng);
        if i == 0 {
            let mut c = w.chars();
            if let Some(first) = c.next() {
                s.extend(first.to_uppercase());
                s.push_str(c.as_str());
            }
        } else {
            s.push(' ');
            s.push_str(&w);
        }
    }
    s.push(if rng.chance(0.1) { '?' } else { '.' });
    s
}

pub fn paragraph(rng: &mut Rng) -> String {
    let n = rng.range(1, 6);
    (0..n).map(|_| sentence(rng)).collect::<Vec<_>>().join(" ")
}

/// Wraps text at 72 columns.
pub fn wrap(text: &str, eol: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 60);
    let mut col = 0;
    for w in text.split(' ') {
        if col > 0 && col + 1 + w.len() > 72 {
            out.push_str(eol);
            col = 0;
        } else if col > 0 {
            out.push(' ');
            col += 1;
        }
        out.push_str(w);
        col += w.len();
    }
    out
}

pub const FIRST: &[&str] = &[
    "Ann", "Bob", "Carol", "David", "Erin", "Frank", "Grace", "Heidi", "Ivan", "Judy", "Ken", "Laura", "Mike",
    "Nina", "Oscar", "Peggy", "Quinn", "Rosa", "Sam", "Tina", "Uma", "Victor", "Wendy", "Xavier", "Yvonne", "Zack",
    "José", "Zoë", "Søren", "Łukasz", "Renée", "Björn", "Chloé", "Mateo", "Priya", "Wei",
];
pub const LAST: &[&str] = &[
    "Smith", "Jones", "Brown", "Lee", "Garcia", "Miller", "Davis", "Wilson", "Moore", "Taylor", "Anderson", "Thomas",
    "Jackson", "White", "Harris", "Martin", "Clark", "Lewis", "Walker", "Hall", "Young", "King", "Wright", "Scott",
    "Pérez", "Müller", "Øster", "Nováková", "O'Brien", "Nakamura",
];
pub const DOMAINS: &[&str] = &[
    "example.com", "example.org", "example.net", "acme.example", "globex.example", "initech.example", "umbrella.example",
    "hooli.example", "corp.example", "oldisp.example", "university.example", "aol.example", "compuserve.example",
];
