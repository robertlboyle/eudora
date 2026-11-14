//! Words to speech sounds (Kokoro's phonemes) for the words misaki's dictionaries don't have:
//! names, companies, jargon, the stuff of email. First the CMU Pronouncing Dictionary (BSD-2,
//! 135,000 words with many names), then the NRL letter-to-sound rules (public domain) for anything
//! else, so a word is always said, never dropped. All GPL-free (no espeak-ng).

use std::collections::HashMap;
use std::sync::OnceLock;

use misaki_rs::fallback::FallbackError;

use crate::nrl_rules::{RULES, Rule};

static CMUDICT_TEXT: &str = include_str!("../data/cmudict.dict");

fn cmudict() -> &'static HashMap<&'static str, &'static str> {
    static DICT: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    DICT.get_or_init(|| {
        let mut map = HashMap::with_capacity(140_000);
        for line in CMUDICT_TEXT.lines() {
            // "word P R AH0 N" (a "#" note may follow); "word(2) …" = another way to say it: the
            // first one is kept
            let line = line.split(" #").next().unwrap_or(line);
            let Some((word, sounds)) = line.split_once(' ') else { continue };
            if word.contains('(') {
                continue;
            }
            map.entry(word).or_insert(sounds.trim());
        }
        map
    })
}

/// One CMU (ARPAbet) sound in Kokoro's symbols; vowels carry their stress.
fn arpabet(sound: &str) -> String {
    let (base, stress) = match sound.char_indices().last() {
        Some((i, d)) if d.is_ascii_digit() => (&sound[..i], Some(d)),
        _ => (sound, None),
    };
    let mark = match stress {
        Some('1') => "ˈ",
        Some('2') => "ˌ",
        _ => "",
    };
    let unstressed = stress == Some('0');
    let vowel = match base {
        "AA" => "ɑ",
        "AE" => "æ",
        "AH" => if unstressed { "ə" } else { "ʌ" },
        "AO" => "ɔ",
        "AW" => "W",
        "AY" => "I",
        "EH" => "ɛ",
        "ER" => if unstressed { "əɹ" } else { "ɜɹ" },
        "EY" => "A",
        "IH" => "ɪ",
        "IY" => "i",
        "OW" => "O",
        "OY" => "Y",
        "UH" => "ʊ",
        "UW" => "u",
        _ => "",
    };
    if !vowel.is_empty() {
        return format!("{mark}{vowel}");
    }
    match base {
        "B" => "b",
        "CH" => "ʧ",
        "D" => "d",
        "DH" => "ð",
        "F" => "f",
        "G" => "ɡ",
        "HH" => "h",
        "JH" => "ʤ",
        "K" => "k",
        "L" => "l",
        "M" => "m",
        "N" => "n",
        "NG" => "ŋ",
        "P" => "p",
        "R" => "ɹ",
        "S" => "s",
        "SH" => "ʃ",
        "T" => "t",
        "TH" => "θ",
        "V" => "v",
        "W" => "w",
        "Y" => "j",
        "Z" => "z",
        "ZH" => "ʒ",
        _ => "",
    }
    .to_string()
}

/// A word from the CMU dictionary, in Kokoro's symbols.
pub fn from_cmudict(word: &str) -> Option<String> {
    let key = word.to_lowercase();
    let sounds = cmudict().get(key.as_str()).or_else(|| {
        // (possessives: "Morgan's" = "Morgan" + z)
        key.strip_suffix("'s").and_then(|w| cmudict().get(w)).map(|_| &"")
    })?;
    if sounds.is_empty() {
        let base = from_cmudict(key.strip_suffix("'s")?)?;
        return Some(format!("{base}z"));
    }
    Some(sounds.split_whitespace().map(arpabet).collect())
}

// ------------------------------------------------------------------ the NRL rules

fn is_vowel(c: u8) -> bool {
    matches!(c, b'A' | b'E' | b'I' | b'O' | b'U')
}

fn is_consonant(c: u8) -> bool {
    c.is_ascii_uppercase() && !is_vowel(c)
}

fn is_voiced(c: u8) -> bool {
    matches!(c, b'B' | b'D' | b'V' | b'G' | b'J' | b'L' | b'M' | b'N' | b'R' | b'W' | b'Z')
}

/// The left context `pattern` matched backward from `at` (the letter before the match).
fn left_matches(pattern: &str, word: &[u8], at: isize) -> bool {
    let mut text = at;
    let get = |i: isize| if i >= 0 && (i as usize) < word.len() { word[i as usize] } else { 0 };
    // (every character of the pattern, last first: the 1985 C skipped the first one)
    for &p in pattern.as_bytes().iter().rev() {
        let c = get(text);
        match p {
            b'#' => {
                if !is_vowel(c) {
                    return false;
                }
                text -= 1;
                while is_vowel(get(text)) {
                    text -= 1;
                }
            }
            b':' => {
                while is_consonant(get(text)) {
                    text -= 1;
                }
            }
            b'^' => {
                if !is_consonant(c) {
                    return false;
                }
                text -= 1;
            }
            b'.' => {
                if !is_voiced(c) {
                    return false;
                }
                text -= 1;
            }
            b'+' => {
                if !matches!(c, b'E' | b'I' | b'Y') {
                    return false;
                }
                text -= 1;
            }
            _ => {
                if p != c {
                    return false;
                }
                text -= 1;
            }
        }
    }
    true
}

/// The right context `pattern` matched forward from `at` (the letter after the match).
fn right_matches(pattern: &str, word: &[u8], at: usize) -> bool {
    let mut text = at;
    let get = |i: usize| word.get(i).copied().unwrap_or(0);
    for &p in pattern.as_bytes() {
        let c = get(text);
        match p {
            b'#' => {
                if !is_vowel(c) {
                    return false;
                }
                text += 1;
                while is_vowel(get(text)) {
                    text += 1;
                }
            }
            b':' => {
                while is_consonant(get(text)) {
                    text += 1;
                }
            }
            b'^' => {
                if !is_consonant(c) {
                    return false;
                }
                text += 1;
            }
            b'.' => {
                if !is_voiced(c) {
                    return false;
                }
                text += 1;
            }
            b'+' => {
                if !matches!(c, b'E' | b'I' | b'Y') {
                    return false;
                }
                text += 1;
            }
            b'%' => {
                // ER, E, ES, ED, ING, ELY
                if c == b'E' {
                    text += 1;
                    match get(text) {
                        b'L' if get(text + 1) == b'Y' => text += 2,
                        b'R' | b'S' | b'D' => text += 1,
                        _ => {}
                    }
                } else if c == b'I' && get(text + 1) == b'N' && get(text + 2) == b'G' {
                    text += 3;
                } else {
                    return false;
                }
            }
            _ => {
                if p != c {
                    return false;
                }
                text += 1;
            }
        }
    }
    true
}

/// A word in NRL's phoneme codes ("bIHz"), by its rules.
fn nrl_codes(word: &str) -> String {
    // " WORD " (spaces mark the word's ends, as the rules expect)
    let letters: String = word.to_uppercase().chars().filter(|c| c.is_ascii_uppercase() || *c == '\'').collect();
    let w = format!(" {letters} ");
    let bytes = w.as_bytes();
    let mut out = String::new();
    let mut i = 1;
    while i < bytes.len() - 1 {
        let c = bytes[i];
        let rules: &[Rule] = if c.is_ascii_uppercase() { RULES[(c - b'A' + 1) as usize] } else { RULES[0] };
        let mut matched = false;
        for (left, text, right, phonemes) in rules {
            let t = text.as_bytes();
            if i + t.len() > bytes.len() || &bytes[i..i + t.len()] != t {
                continue;
            }
            if !left_matches(left, bytes, i as isize - 1) || !right_matches(right, bytes, i + t.len()) {
                continue;
            }
            out.push_str(phonemes);
            i += t.len().max(1);
            matched = true;
            break;
        }
        if !matched {
            i += 1;
        }
    }
    out
}

/// NRL's codes in Kokoro's symbols, with the stress on the first full vowel.
fn nrl_to_kokoro(codes: &str) -> String {
    let b = codes.as_bytes();
    let mut out = String::new();
    let mut stressed = false;
    let mut i = 0;
    while i < b.len() {
        let two = if i + 1 < b.len() && b[i].is_ascii_uppercase() && b[i + 1].is_ascii_uppercase() { Some(&codes[i..i + 2]) } else { None };
        let (sound, vowel, len) = match two {
            Some(code) => {
                let (s, v) = match code {
                    "IY" => ("i", true),
                    "IH" => ("ɪ", true),
                    "EY" => ("A", true),
                    "EH" => ("ɛ", true),
                    "AE" => ("æ", true),
                    "AA" => ("ɑ", true),
                    "AO" => ("ɔ", true),
                    "OW" => ("O", true),
                    "UH" => ("ʊ", true),
                    "UW" => ("u", true),
                    "ER" => ("ɜɹ", true),
                    "AX" => ("ə", false),
                    "AH" => ("ʌ", true),
                    "AY" => ("I", true),
                    "AW" => ("W", true),
                    "OY" => ("Y", true),
                    "TH" => ("θ", false),
                    "DH" => ("ð", false),
                    "SH" => ("ʃ", false),
                    "ZH" => ("ʒ", false),
                    "HH" => ("h", false),
                    "NG" => ("ŋ", false),
                    "CH" => ("ʧ", false),
                    "WH" => ("w", false),
                    _ => ("", false),
                };
                (s, v, 2)
            }
            None => {
                let s = match b[i] {
                    b'p' => "p",
                    b'b' => "b",
                    b't' => "t",
                    b'd' => "d",
                    b'k' => "k",
                    b'g' => "ɡ",
                    b'f' => "f",
                    b'v' => "v",
                    b's' => "s",
                    b'z' => "z",
                    b'm' => "m",
                    b'n' => "n",
                    b'l' => "l",
                    b'w' => "w",
                    b'y' => "j",
                    b'r' => "ɹ",
                    b'j' => "ʤ",
                    _ => "",
                };
                (s, false, 1)
            }
        };
        if vowel && !stressed {
            out.push('ˈ');
            stressed = true;
        }
        out.push_str(sound);
        i += len;
    }
    out
}

/// A word by the NRL rules, in Kokoro's symbols.
pub fn from_rules(word: &str) -> String {
    nrl_to_kokoro(&nrl_codes(word))
}

/// Letters said one by one ("DQS" = dee, cue, ess), from the CMU dictionary's letter names.
fn spelled(word: &str) -> Option<String> {
    let parts: Option<Vec<String>> = word.chars().filter(|c| c.is_alphanumeric()).map(|c| from_cmudict(&c.to_string())).collect();
    parts.map(|p| p.join(" "))
}

/// Misaki's fallback: a word it doesn't know.
pub struct Words;

impl misaki_rs::Fallback for Words {
    fn phonemize(&self, word: &str) -> Result<String, FallbackError> {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'');
        if word.is_empty() {
            return Err(FallbackError::NoPhonemes { word: word.to_string() });
        }
        if let Some(p) = from_cmudict(word) {
            return Ok(p);
        }
        // a short word in capitals with no vowel to say ("DQS", "NJUNS" is said as a word)
        let caps = word.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if caps && (word.len() <= 3 || !word.chars().any(|c| "AEIOUY".contains(c))) {
            if let Some(p) = spelled(word) {
                return Ok(p);
            }
        }
        // a word made of two known ones ("Spamhaus" is not, "mailbox" would be)
        let lower = word.to_lowercase();
        for cut in (3..lower.len().saturating_sub(2)).rev() {
            if !lower.is_char_boundary(cut) {
                continue;
            }
            if let (Some(a), Some(b)) = (from_cmudict(&lower[..cut]), from_cmudict(&lower[cut..])) {
                return Ok(format!("{a}{}", b.replace('ˈ', "ˌ")));
            }
        }
        let p = from_rules(word);
        if p.is_empty() {
            return Err(FallbackError::NoPhonemes { word: word.to_string() });
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_cmu_dictionary() {
        assert_eq!(from_cmudict("Doyle").as_deref(), Some("dˈYl"));
        assert_eq!(from_cmudict("Kristin").as_deref(), Some("kɹˈɪstɪn"));
        assert_eq!(from_cmudict("Doyle's").as_deref(), Some("dˈYlz"));
    }

    #[test]
    fn unknown_words_are_said_by_the_rules() {
        // never empty, and the usual English guesses
        for w in ["Spamhaus", "Tantivy", "Zorblax", "Qwerp"] {
            assert!(!from_rules(w).is_empty(), "{w}");
        }
        assert_eq!(from_rules("cat"), "kˈæt");
        assert_eq!(from_rules("ship"), "ʃˈɪp");
    }
}
