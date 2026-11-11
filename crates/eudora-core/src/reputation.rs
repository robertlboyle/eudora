//! Domain reputation: how old a sender's domain is, from the public registries (RDAP: a "bank"
//! registered last week is suspicious), and the user's own history with it (when its mail first
//! came). Looked up only for new mail, once per domain, and kept (`domains`): a registration
//! date doesn't change.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};

use crate::error::Result;
use crate::library::{Library, now};

/// A domain registered less than this long ago is young (and less than a week: very young).
pub const YOUNG: i64 = 30 * 86_400;
pub const VERY_YOUNG: i64 = 7 * 86_400;
/// A failed lookup is tried again after this long.
const RETRY_AFTER: i64 = 7 * 86_400;

/// What's known of a domain.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DomainFacts {
    /// When it was registered (unix seconds), if the registry says.
    pub registered: Option<i64>,
    /// When its mail first came to the library.
    pub first_seen: Option<i64>,
}

/// The registration date in an RDAP answer (its "registration" event).
fn registration(json: &serde_json::Value) -> Option<i64> {
    json["events"].as_array()?.iter().find(|e| e["eventAction"].as_str() == Some("registration")).and_then(|e| e["eventDate"].as_str()).and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok()).map(|d| d.timestamp())
}

/// Asks the registries (rdap.org sends the question on to the registry that knows).
fn rdap(domain: &str) -> Option<i64> {
    let config = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(8))).user_agent("Eudora").build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = agent.get(&format!("https://rdap.org/domain/{domain}")).header("Accept", "application/rdap+json").call().ok()?;
    let json: serde_json::Value = response.body_mut().read_json().ok()?;
    registration(&json)
}

impl Library {
    /// What's known of these organization domains (from the table; nothing is looked up).
    pub fn domain_facts(&self, domains: &[String]) -> Result<HashMap<String, DomainFacts>> {
        let mut out = HashMap::new();
        let mut stmt = self.conn.prepare_cached("SELECT registered, first_seen FROM domains WHERE domain = ?1")?;
        for d in domains {
            if let Some(f) = stmt.query_row(params![d], |r| Ok(DomainFacts { registered: r.get(0)?, first_seen: r.get(1)? })).optional()? {
                out.insert(d.clone(), f);
            }
        }
        Ok(out)
    }

    /// Looks up the domains not known yet (or whose lookup failed a while ago): their
    /// registration (RDAP, several at a time) and the user's first mail from them. For new
    /// mail, just before it's sorted.
    pub fn learn_domains(&self, domains: &[String]) -> Result<u64> {
        let t = now();
        let mut todo: Vec<String> = Vec::new();
        {
            let mut stmt = self.conn.prepare_cached("SELECT registered, checked FROM domains WHERE domain = ?1")?;
            for d in domains {
                if d.is_empty() || !d.contains('.') || todo.contains(d) {
                    continue;
                }
                let known: Option<(Option<i64>, i64)> = stmt.query_row(params![d], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                match known {
                    Some((Some(_), _)) => {}
                    Some((None, checked)) if t - checked < RETRY_AFTER => {}
                    _ => todo.push(d.clone()),
                }
            }
        }
        if todo.is_empty() {
            return Ok(0);
        }
        use rayon::prelude::*;
        let found: Vec<(String, Option<i64>)> = todo.par_iter().map(|d| (d.clone(), rdap(d))).collect();
        for (domain, registered) in &found {
            // the user's first mail from it (from any address at it or under it)
            let first: Option<i64> = self.conn.query_row(
                "SELECT MIN(m.date) FROM addresses a JOIN message_addresses ma ON ma.address = a.id AND ma.role = 0 JOIN messages m ON m.id = ma.message
                 WHERE (a.address LIKE '%@' || ?1 OR a.address LIKE '%.' || ?1) AND m.date > 0",
                params![domain],
                |r| r.get(0),
            )?;
            self.conn.execute(
                "INSERT INTO domains (domain, registered, first_seen, checked) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (domain) DO UPDATE SET registered = COALESCE(excluded.registered, registered), first_seen = excluded.first_seen, checked = excluded.checked",
                params![domain, registered, first, t],
            )?;
        }
        Ok(found.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registration_date_is_read_from_an_rdap_answer() {
        let json = serde_json::json!({ "events": [
            { "eventAction": "last changed", "eventDate": "2026-01-01T00:00:00Z" },
            { "eventAction": "registration", "eventDate": "1997-09-15T04:00:00Z" },
        ]});
        assert_eq!(registration(&json), Some(874_296_000));
        assert_eq!(registration(&serde_json::json!({})), None);
    }
}
