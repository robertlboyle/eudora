//! Subscriptions: every legitimate mailing list, marketing and notification sender in one place,
//! and a way out of each.
//!
//! A subscription = mail carrying an unsubscribe header (List-Unsubscribe) in the last year,
//! grouped by its mailing list (List-Id) or else its sender's organization. Only senders that
//! prove who they are (DMARC or a DKIM signature) and that the spam filter and the fraud check
//! don't flag are listed: unsubscribing from spam only tells the spammer the address is read.
//!
//! Unsubscribing: the standard one-click (RFC 8058: List-Unsubscribe-Post) is done by Eudora; a
//! web page is opened in the browser; an address gets a message the user sends. Each one is
//! remembered (setting "unsubscribed"), so mail that keeps coming is plain to see.

use std::collections::HashMap;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::library::{Library, now};

/// How far back a subscription is looked for.
const YEAR: i64 = 365 * 86_400;

/// Newsletter and group services that send many people's lists from one domain: each sender
/// address there is a subscription of its own.
const PLATFORMS: &[&str] = &[
    "substack.com", "beehiiv.com", "ghost.io", "buttondown.email", "buttondown.com", "convertkit.com", "kit.com", "mailchi.mp", "groups.io",
    "googlegroups.com", "medium.com", "patreon.com", "tinyletter.com", "revue.email", "mailerlite.com", "flodesk.com",
];

/// How to leave one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "kebab-case")]
pub enum Way {
    /// Eudora does it: an HTTPS POST to this address (RFC 8058).
    OneClick { url: String },
    /// The sender's unsubscribe / manage-subscription page, opened in the browser.
    Page { url: String },
    /// A message to this address (the user sends it).
    Email { address: String, subject: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct Subscription {
    /// What groups it: its List-Id, else its sender's organization domain (or address).
    pub key: String,
    /// The sender's name as its newest message shows it.
    pub name: String,
    pub address: String,
    pub list_id: Option<String>,
    /// Messages in the last year, and in the last 30 days.
    pub year: u64,
    pub month: u64,
    pub first: i64,
    pub last: i64,
    /// The newest message (what the way out is read from).
    pub newest: i64,
    /// The mailbox it arrives in (its newest message's account).
    pub mailbox: String,
    /// Marketing, notification or mailing list (the spam filter's reading of its newest message).
    pub kind: String,
    pub way: Option<Way>,
    /// When the user unsubscribed (and mail since: still coming).
    pub unsubscribed: Option<i64>,
    pub since_unsubscribed: u64,
}

/// What `unsubscribe` did, or what the window does next.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "done", rename_all = "kebab-case")]
pub enum Outcome {
    /// Eudora unsubscribed (one-click): the sender's answer.
    Unsubscribed { status: u16 },
    /// The window opens this page in the browser.
    OpenPage { url: String },
    /// The window opens a message to this address, ready to send.
    WriteTo { address: String, subject: String },
}

/// The way out a message offers, best first: one-click, then a page, then an address.
pub fn way_out(targets: &[String], one_click: bool) -> Option<Way> {
    let https = targets.iter().find(|t| t.to_ascii_lowercase().starts_with("https://"));
    let http = targets.iter().find(|t| t.to_ascii_lowercase().starts_with("http://"));
    let mail = targets.iter().find(|t| t.to_ascii_lowercase().starts_with("mailto:"));
    match (https, http, mail) {
        (Some(url), _, _) if one_click => Some(Way::OneClick { url: url.clone() }),
        (Some(url), _, _) | (None, Some(url), _) => Some(Way::Page { url: url.clone() }),
        (None, None, Some(m)) => {
            let rest = &m[7..];
            let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
            let subject = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("subject=").or_else(|| kv.strip_prefix("Subject=")))
                .map(|s| s.replace("%20", " ").replace('+', " "))
                .unwrap_or_else(|| "unsubscribe".into());
            Some(Way::Email { address: address.to_string(), subject })
        }
        _ => None,
    }
}

impl Library {
    /// Every legitimate subscription with mail in the last year, the busiest first.
    pub fn subscriptions(&self) -> Result<Vec<Subscription>> {
        let Some(headers) = &self.headers else { return Ok(Vec::new()) };
        let clause = crate::header_index::Clause { name: "list-unsubscribe".into(), words: Vec::new(), negate: false };
        let since = now() - YEAR;
        // the last year's messages that have an unsubscribe link (the year's ids first: the
        // header index holds every year)
        let recent: std::collections::HashSet<u64> = {
            let mut stmt = self.conn.prepare("SELECT id FROM messages WHERE date >= ?1")?;
            stmt.query_map(params![since], |r| r.get::<_, i64>(0))?.map(|r| r.map(|i| i as u64)).collect::<rusqlite::Result<_>>()?
        };
        let ids: Vec<u64> = headers.matching_ids(&[clause])?.into_iter().filter(|i| recent.contains(i)).collect();
        self.conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS sub_ids (id INTEGER PRIMARY KEY); DELETE FROM temp.sub_ids;")?;
        {
            let mut add = self.conn.prepare_cached("INSERT OR IGNORE INTO temp.sub_ids VALUES (?1)")?;
            self.conn.execute_batch("BEGIN")?;
            for id in &ids {
                add.execute(params![*id as i64])?;
            }
            self.conn.execute_batch("COMMIT")?;
        }
        // each message's date, sender and list; the user's own mailboxes only (a shared mailbox's
        // newsletters are someone else's to leave)
        let rows: Vec<(i64, i64, String, String, Option<String>)> = {
            let mut stmt = self.conn.prepare(
                "SELECT m.id, m.date, COALESCE(a.address, ''), m.from_display, m.list_id FROM temp.sub_ids s JOIN messages m ON m.id = s.id
                 LEFT JOIN message_addresses ma ON ma.message = m.id AND ma.role = 0 AND ma.position = 0 LEFT JOIN addresses a ON a.id = ma.address
                 WHERE m.date >= ?1 AND EXISTS (SELECT 1 FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts ac ON ac.id = f.account
                                                 WHERE l.message = m.id AND ac.kind <> 'graph-shared')",
            )?;
            stmt.query_map(params![since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let own: Vec<String> = self.own_addresses()?.into_iter().map(|a| a.to_lowercase()).collect();
        let month_ago = now() - 30 * 86_400;
        let mut groups: HashMap<String, Subscription> = HashMap::new();
        for (id, date, from, display, list_id) in rows {
            let from = from.to_lowercase();
            if from.is_empty() || own.contains(&from) {
                continue;
            }
            let key = match &list_id {
                Some(l) if !l.is_empty() => l.to_lowercase(),
                _ => {
                    let domain = from.rsplit('@').next().unwrap_or("");
                    let org = crate::triage_decide::org_domain(domain);
                    if org.is_empty() || crate::triage_decide::shared(&org) || PLATFORMS.contains(&org.as_str()) { from.clone() } else { org }
                }
            };
            let g = groups.entry(key.clone()).or_insert_with(|| Subscription {
                key,
                name: String::new(),
                address: String::new(),
                list_id: list_id.clone(),
                year: 0,
                month: 0,
                first: date,
                last: 0,
                newest: id,
                mailbox: String::new(),
                kind: String::new(),
                way: None,
                unsubscribed: None,
                since_unsubscribed: 0,
            });
            g.year += 1;
            if date >= month_ago {
                g.month += 1;
            }
            g.first = g.first.min(date);
            if date >= g.last {
                g.last = date;
                g.newest = id;
                g.address = from.clone();
                g.name = display.clone();
            }
        }
        let unsubscribed = self.unsubscribed()?;
        let mut out = Vec::new();
        let mut category = self.conn.prepare_cached("SELECT category FROM triage WHERE message = ?1")?;
        // each sender's newest message read (in parallel): only the legitimate are listed, a
        // proven sender that isn't a fraud
        let checked: HashMap<i64, (bool, Option<Way>, &'static str)> = {
            use rayon::prelude::*;
            let mut locate = self.conn.prepare_cached("SELECT pack, pack_offset, pack_len FROM messages WHERE id = ?1")?;
            let places: Vec<(i64, crate::pack::Location)> =
                groups.values().filter_map(|g| locate.query_row(params![g.newest], |x| Library::location(x, 0)).ok().map(|l| (g.newest, l))).collect();
            let allowed: std::collections::HashSet<String> = {
                let mut stmt = self.conn.prepare("SELECT pattern FROM sender_rules WHERE kind = 'allow'")?;
                stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?
            };
            let mut own_domains: Vec<String> = own.iter().filter_map(|a| a.split('@').nth(1).map(str::to_string)).collect();
            own_domains.sort();
            own_domains.dedup();
            let packs = &self.packs;
            places
                .par_iter()
                .filter_map(|(id, at)| {
                    let raw = packs.read(*at).ok()?.raw;
                    let headers = crate::header_index::read_headers(&raw);
                    let s = crate::triage::signals(&headers);
                    let proven = s.dmarc.as_deref() == Some("pass") || s.dkim.as_deref() == Some("pass");
                    if !proven {
                        return Some((*id, (false, None, "")));
                    }
                    let from = s.from.to_lowercase();
                    let trusted = allowed.contains(&from) || from.rsplit_once('@').is_some_and(|(_, d)| allowed.contains(&format!("@{d}")));
                    let subject = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("Subject")).map(|(_, v)| v.clone()).unwrap_or_default();
                    let text = crate::parse::parse(&raw, crate::parse::DateHints::default(), now()).body_text;
                    let fraud = crate::phishing::warning(&s, &subject, &text, &own_domains, trusted).is_some();
                    // its kind when the spam filter hasn't sorted it: a list by its List-Id, a
                    // notification by a well-known sender or its subject's words
                    let kind = if s.list_id.is_some() {
                        "Mailing list"
                    } else if crate::triage_known::hint(&s, &subject, true, true).is_some_and(|h| h.kind == crate::triage_known::Kind::Notification) {
                        "Notifications"
                    } else {
                        "Marketing"
                    };
                    Some((*id, (!fraud, way_out(&s.unsubscribe_targets, s.one_click), kind)))
                })
                .collect()
        };
        for mut g in groups.into_values() {
            let Some((true, way, kind)) = checked.get(&g.newest).cloned() else { continue };
            let decided: Option<String> = category.query_row(params![g.newest], |r| r.get(0)).ok();
            if matches!(decided.as_deref(), Some("spam" | "blocked")) {
                continue;
            }
            g.kind = match decided.as_deref() {
                Some("list") => "Mailing list".into(),
                Some("notification") => "Notifications".into(),
                Some("bulk") => "Marketing".into(),
                _ => kind.into(),
            };
            g.way = way;
            g.mailbox = self.message_mailbox(g.newest)?;
            if let Some(at) = unsubscribed.get(&g.key) {
                g.unsubscribed = Some(*at);
                g.since_unsubscribed = self.count_since(&g, *at)?;
            }
            out.push(g);
        }
        out.sort_by(|a, b| b.year.cmp(&a.year).then(b.last.cmp(&a.last)));
        Ok(out)
    }

    /// Messages from a subscription since a time (unsubscribed, yet still coming).
    fn count_since(&self, g: &Subscription, at: i64) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM temp.sub_ids s JOIN messages m ON m.id = s.id
             LEFT JOIN message_addresses ma ON ma.message = m.id AND ma.role = 0 AND ma.position = 0 LEFT JOIN addresses a ON a.id = ma.address
             WHERE m.date > ?1 AND (lower(COALESCE(m.list_id, '')) = ?2 OR lower(COALESCE(a.address, '')) = ?3 OR lower(COALESCE(a.address, '')) LIKE '%@' || ?2 OR lower(COALESCE(a.address, '')) LIKE '%.' || ?2)",
            params![at, g.key, g.address],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// When each subscription was left (key → unix time).
    pub fn unsubscribed(&self) -> Result<HashMap<String, i64>> {
        Ok(self.setting("unsubscribed")?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    /// Leaves a subscription by its newest message's way out (checked again here: never for
    /// spam or a fraud). One-click is done now; a page or an address goes back to the window.
    pub fn unsubscribe(&self, key: &str, newest: i64) -> Result<Outcome> {
        let raw = self.raw_message(newest)?;
        let headers = crate::header_index::read_headers(&raw);
        let s = crate::triage::signals(&headers);
        let proven = s.dmarc.as_deref() == Some("pass") || s.dkim.as_deref() == Some("pass");
        if !proven || self.fraud_warning(&raw)?.is_some() {
            return Err(Error::Other("this sender doesn't prove who it is, or looks like a fraud: unsubscribing would only tell it your address is read. Mark it Spam instead.".into()));
        }
        let outcome = match way_out(&s.unsubscribe_targets, s.one_click) {
            None => return Err(Error::Other("its mail has no way to unsubscribe".into())),
            Some(Way::OneClick { url }) => {
                // (certificates checked by the system, as for every connection Eudora makes)
                let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
                let agent = ureq::Agent::config_builder()
                    .tls_config(tls)
                    .http_status_as_error(false)
                    .timeout_global(Some(std::time::Duration::from_secs(30)))
                    .user_agent(concat!("Eudora/27 (eudora-core ", env!("CARGO_PKG_VERSION"), ")"))
                    .build()
                    .new_agent();
                let response = agent
                    .post(&url)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .send("List-Unsubscribe=One-Click")
                    .map_err(|e| Error::Other(format!("the sender's unsubscribe didn't answer ({e}); try its page instead")))?;
                Outcome::Unsubscribed { status: response.status().as_u16() }
            }
            Some(Way::Page { url }) => Outcome::OpenPage { url },
            Some(Way::Email { address, subject }) => Outcome::WriteTo { address, subject },
        };
        let mut done = self.unsubscribed()?;
        done.insert(key.to_string(), now());
        self.set_setting("unsubscribed", &serde_json::to_string(&done)?)?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_best_way_out_is_chosen() {
        let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(way_out(&t(&["mailto:u@x.example", "https://x.example/u?a=1"]), true), Some(Way::OneClick { url: "https://x.example/u?a=1".into() }));
        assert_eq!(way_out(&t(&["mailto:u@x.example", "https://x.example/u"]), false), Some(Way::Page { url: "https://x.example/u".into() }));
        assert_eq!(
            way_out(&t(&["mailto:leave@lists.example?subject=unsubscribe%20me"]), false),
            Some(Way::Email { address: "leave@lists.example".into(), subject: "unsubscribe me".into() })
        );
        assert_eq!(way_out(&[], false), None);
    }
}
