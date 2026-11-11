//! DNS blocklists (the project's design notes step 5): Spamhaus, through its DQS service with the user's
//! own key (entered in Settings, kept in the system's credential store, never shipped, never
//! shown). For **new mail only**, at arrival: listings are point-in-time, so old mail isn't
//! checked. Asked through the system's own resolver (the key in the name is what lets DQS
//! answer). Answers are kept an hour (`blocklist_cache`).
//!
//! - ZEN: the IP address that sent it (Microsoft's `CIP:`, else the first outside `Received`
//!   hop): 127.0.0.2-3 SBL, 4-7 XBL (hijacked machines), 9 DROP, 10-11 PBL (addresses that
//!   shouldn't send mail directly).
//! - DBL: the sender's domain: 127.0.1.2-99 (spam, phishing, malware… domains).
//! - ZRD: domains first seen in the last 24 hours: 127.0.2.2-24 (hours since).
//!
//! 127.255.255.x answers mean the query was refused or the key is wrong: never "listed".

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};

use rusqlite::params;

use crate::error::Result;
use crate::library::{Library, now};

const SECRET: &str = "spamhaus-dqs";
const KEEP: i64 = 3600;

/// The user's Spamhaus DQS key, if one was entered on this computer.
pub fn key() -> Option<String> {
    crate::graph::load_secret(SECRET).and_then(|b| String::from_utf8(b).ok()).map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

/// Keeps the key (an empty one forgets it).
pub fn set_key(key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        crate::graph::delete_secret(SECRET);
        return Ok(());
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(crate::Error::Other("a Spamhaus DQS key is letters and digits only".into()));
    }
    crate::graph::store_secret(SECRET, key.as_bytes())
}

/// One A-record answer for `name` (None: not listed, or no answer).
fn ask(name: &str) -> Option<Ipv4Addr> {
    (name, 0).to_socket_addrs().ok()?.find_map(|a| match a.ip() {
        IpAddr::V4(v4) => Some(v4),
        _ => None,
    })
}

/// What an answer means for a list (None: not listed, or "unavailable").
fn meaning(list: &str, answer: Ipv4Addr) -> Option<String> {
    let [a, b, c, d] = answer.octets();
    if a != 127 || (b == 255 && c == 255) {
        return None; // refused / bad key / not a listing
    }
    match (list, c) {
        ("zen", 0) => Some(match d {
            2 | 3 => "SBL (a known spam source)".into(),
            4..=7 => "XBL (a hijacked or infected machine)".into(),
            9 => "DROP (a hijacked network)".into(),
            10 | 11 => "PBL (an address that shouldn't send mail directly)".into(),
            _ => format!("listed ({d})"),
        }),
        ("dbl", 1) => Some(match d {
            2 => "a spam domain".into(),
            4 => "a phishing domain".into(),
            5 => "a malware domain".into(),
            6 => "a botnet controller".into(),
            _ => format!("a listed domain ({d})"),
        }),
        ("zrd", 2) => Some(format!("first seen {} hour{} ago", d.saturating_sub(2), if d == 3 { "" } else { "s" })),
        _ => None,
    }
}

/// The IP address the mail came from: Microsoft's record (`CIP:`), else the first `Received`
/// hop that's a public address.
pub fn sending_ip(headers: &[(String, String)]) -> Option<Ipv4Addr> {
    if let Some(report) = headers.iter().find(|(n, _)| n == "x-forefront-antispam-report") {
        if let Some(ip) = report.1.split(';').find_map(|f| f.trim().strip_prefix("CIP:")).and_then(|ip| ip.trim().parse::<Ipv4Addr>().ok()) {
            return Some(ip);
        }
    }
    for (_, v) in headers.iter().filter(|(n, _)| n == "received").rev() {
        for piece in v.split(['[', ']', '(', ')', ' ']) {
            if let Ok(ip) = piece.parse::<Ipv4Addr>() {
                if !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()) {
                    return Some(ip);
                }
            }
        }
    }
    None
}

/// The names a message is checked by: (list, name looked up as).
fn names(ip: Option<Ipv4Addr>, domain: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(ip) = ip {
        let [a, b, c, d] = ip.octets();
        out.push(("zen", format!("{d}.{c}.{b}.{a}")));
    }
    if domain.contains('.') {
        out.push(("dbl", domain.to_string()));
        out.push(("zrd", domain.to_string()));
    }
    out
}

impl Library {
    /// Checks new messages (their headers) against the lists, when a key is set; the answers
    /// are kept for deciding. Returns how many names were asked.
    pub fn check_blocklists(&self, ids: &[i64]) -> Result<u64> {
        let Some(key) = key() else { return Ok(0) };
        let mut todo: Vec<(&'static str, String)> = Vec::new();
        let mut stmt = self.conn.prepare_cached("SELECT pack, pack_offset, pack_len FROM messages WHERE id = ?1")?;
        let mut fresh = self.conn.prepare_cached("SELECT 1 FROM blocklist_cache WHERE list = ?1 AND name = ?2 AND expires > ?3")?;
        let t = now();
        for id in ids {
            let Ok(location) = stmt.query_row(params![id], |r| Library::location(r, 0)) else { continue };
            let raw = self.packs.read(location)?.raw;
            let headers = crate::header_index::read_headers(&raw);
            let s = crate::triage::signals(&headers);
            let domain = crate::triage_decide::org_domain(&s.from_domain);
            for (list, name) in names(sending_ip(&headers), &domain) {
                if !fresh.exists(params![list, name, t])? && !todo.iter().any(|(l, n)| *l == list && *n == name) {
                    todo.push((list, name));
                }
            }
        }
        use rayon::prelude::*;
        let answers: Vec<(&str, String, Option<String>)> = todo
            .par_iter()
            .map(|(list, name)| (*list, name.clone(), ask(&format!("{name}.{key}.{list}.dq.spamhaus.net")).and_then(|a| meaning(list, a))))
            .collect();
        let mut keep = self.conn.prepare_cached("INSERT OR REPLACE INTO blocklist_cache (list, name, result, expires) VALUES (?1, ?2, ?3, ?4)")?;
        for (list, name, result) in &answers {
            keep.execute(params![list, name, result, t + KEEP])?;
        }
        Ok(answers.len() as u64)
    }

    /// The listings known now (list, name) → what it means, for deciding.
    pub fn blocklist_listings(&self) -> Result<HashMap<(String, String), String>> {
        let mut stmt = self.conn.prepare("SELECT list, name, result FROM blocklist_cache WHERE result IS NOT NULL AND expires > ?1")?;
        Ok(stmt.query_map(params![now()], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }
}

/// A message's listings, from what's known (for the decision): (rule, spam weight, why).
pub fn listings_for(headers: &[(String, String)], domain: &str, known: &HashMap<(String, String), String>) -> Vec<(&'static str, f32, String)> {
    let mut out = Vec::new();
    for (list, name) in names(sending_ip(headers), domain) {
        let Some(what) = known.get(&(list.to_string(), name.clone())) else { continue };
        match list {
            "zen" if what.starts_with("PBL") => out.push(("spamhaus-ip", 1.5, format!("the server that sent it is on Spamhaus's list: {what}"))),
            "zen" => out.push(("spamhaus-ip", 4.0, format!("the server that sent it is on Spamhaus's list: {what}"))),
            "dbl" => out.push(("spamhaus-domain", 5.0, format!("{domain} is on Spamhaus's domain list: {what}"))),
            "zrd" => out.push(("spamhaus-new-domain", 2.5, format!("{domain} is brand new to the Internet ({what}, Spamhaus)"))),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_mean_listings_and_refusals_never_do() {
        assert_eq!(meaning("zen", Ipv4Addr::new(127, 0, 0, 4)).as_deref(), Some("XBL (a hijacked or infected machine)"));
        assert_eq!(meaning("dbl", Ipv4Addr::new(127, 0, 1, 4)).as_deref(), Some("a phishing domain"));
        assert!(meaning("zrd", Ipv4Addr::new(127, 0, 2, 5)).unwrap().starts_with("first seen 3 hours"));
        assert_eq!(meaning("zen", Ipv4Addr::new(127, 255, 255, 254)), None, "refused: never listed");
        assert_eq!(meaning("dbl", Ipv4Addr::new(127, 255, 255, 252)), None);
        assert_eq!(meaning("zen", Ipv4Addr::new(10, 0, 0, 2)), None);
    }

    #[test]
    fn the_sending_ip_is_microsofts_record_else_the_first_public_hop() {
        let h = |pairs: &[(&str, &str)]| pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect::<Vec<_>>();
        assert_eq!(sending_ip(&h(&[("x-forefront-antispam-report", "CIP:203.0.113.9;CTRY:US;SFV:NSPM")])), Some(Ipv4Addr::new(203, 0, 113, 9)));
        let received = h(&[("received", "from inner (10.0.0.5) by mx"), ("received", "from mail.spam.example (mail.spam.example [198.51.100.7]) by edge")]);
        assert_eq!(sending_ip(&received), Some(Ipv4Addr::new(198, 51, 100, 7)));
        let known: HashMap<(String, String), String> = [(("dbl".to_string(), "bad.example".to_string()), "a phishing domain".to_string())].into();
        let hits = listings_for(&[], "bad.example", &known);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].0, hits[0].1), ("spamhaus-domain", 5.0));
    }
}
