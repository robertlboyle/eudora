//! Sorting mail out of the Inbox and surfacing what matters ("triage").
//!
//! This part: the whitelist and blacklist (`sender_rules`), the automatic whitelist built from
//! the user's own mail (everyone they wrote to, everyone on a message they replied to or
//! forwarded), and the signals a message's headers carry (mailing list, bulk sender, the
//! server's own spam verdict, authentication results). Scoring and moving come on top.

use std::collections::{HashMap, HashSet};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::library::now;
use crate::Library;

/// What a blacklist entry does with a message from that sender (each entry its own).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "lowercase")]
pub enum Action {
    /// Into this folder of the message's account ("Ignored by rule" by default).
    Move { folder: String },
    /// To the account's Trash.
    Trash,
    /// To the account's Junk / Spam folder.
    Spam,
    /// Stays where it is, with this label.
    Label { label: String },
}

/// The folder blacklisted mail goes to unless an entry says otherwise.
pub const IGNORED_FOLDER: &str = "Ignored by rule";

impl Default for Action {
    fn default() -> Self {
        Action::Move { folder: IGNORED_FOLDER.into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SenderRule {
    pub id: i64,
    /// `someone@example.com`, or `@example.com` (the domain and its subdomains).
    pub pattern: String,
    /// allow (whitelist), block (blacklist), not-allow (taken out of the automatic whitelist).
    pub kind: String,
    /// Blacklist entries: what to do.
    pub action: Option<Action>,
    /// auto (from the user's mail) or user.
    pub source: String,
    pub note: String,
    pub added: i64,
    /// The mailbox (account address, lowercase) the entry applies in; empty = every mailbox.
    #[serde(default)]
    pub mailbox: String,
}

/// What rebuilding the automatic whitelist found.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct WhitelistReport {
    /// People the user sent mail to.
    pub written_to: u64,
    /// People on messages the user replied to or forwarded (sender and other recipients).
    pub replied_or_forwarded: u64,
    /// Added to the whitelist this time.
    pub added: u64,
    /// In the automatic whitelist now.
    pub total: u64,
}

/// A whitelist / blacklist pattern as stored: lowercase, an address or `@domain`.
pub fn normalize_pattern(text: &str) -> Result<String> {
    let t = text.trim().trim_matches(['<', '>']).to_lowercase();
    let ok = if let Some(domain) = t.strip_prefix('@') {
        domain.contains('.') && !domain.contains('@')
    } else {
        t.split_once('@').is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.contains('@'))
    };
    if ok { Ok(t) } else { Err(Error::Other(format!("“{text}” isn't an address (name@example.com) or a domain (@example.com)"))) }
}

/// Whether `address` is covered by `pattern` (an address, or `@domain` including subdomains).
pub fn pattern_matches(pattern: &str, address: &str) -> bool {
    let address = address.to_lowercase();
    match pattern.strip_prefix('@') {
        Some(domain) => address.rsplit_once('@').is_some_and(|(_, d)| d == domain || d.ends_with(&format!(".{domain}"))),
        None => address == pattern,
    }
}

impl Library {
    pub fn sender_rules(&self) -> Result<Vec<SenderRule>> {
        let mut stmt = self.conn.prepare("SELECT id, pattern, kind, action, source, note, added, mailbox FROM sender_rules ORDER BY kind, pattern, mailbox")?;
        let rows = stmt.query_map([], |r| {
            let action: String = r.get(3)?;
            Ok(SenderRule {
                id: r.get(0)?,
                pattern: r.get(1)?,
                kind: r.get(2)?,
                action: serde_json::from_str(&action).ok(),
                source: r.get(4)?,
                note: r.get(5)?,
                added: r.get(6)?,
                mailbox: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds (or changes) a whitelist (`allow`) or blacklist (`block`, with its action) entry.
    /// Whitelisting someone takes them off the blacklist and the other way round.
    pub fn set_sender_rule(&self, pattern: &str, kind: &str, action: Option<&Action>, note: &str) -> Result<i64> {
        self.set_sender_rule_in(pattern, kind, action, note, "")
    }

    /// The same, in one mailbox only (`mailbox`: its account's address; empty = every mailbox).
    /// Whitelisting someone in a mailbox takes them off that mailbox's blacklist (and the
    /// other way round); entries for every mailbox stay.
    pub fn set_sender_rule_in(&self, pattern: &str, kind: &str, action: Option<&Action>, note: &str, mailbox: &str) -> Result<i64> {
        if kind != "allow" && kind != "block" {
            return Err(Error::Other(format!("unknown list {kind}")));
        }
        let pattern = normalize_pattern(pattern)?;
        let mailbox = mailbox.trim().to_lowercase();
        let opposite = if kind == "allow" { "block" } else { "allow" };
        let action = match (kind, action) {
            ("block", Some(a)) => serde_json::to_string(a)?,
            ("block", None) => serde_json::to_string(&Action::default())?,
            _ => String::new(),
        };
        self.conn.execute("DELETE FROM sender_rules WHERE pattern = ?1 AND kind IN (?2, 'not-allow') AND mailbox = ?3", params![pattern, opposite, mailbox])?;
        self.conn.execute(
            "INSERT INTO sender_rules (pattern, kind, action, source, note, added, mailbox) VALUES (?1, ?2, ?3, 'user', ?4, ?5, ?6)
             ON CONFLICT (pattern, kind, mailbox) DO UPDATE SET action = excluded.action, note = excluded.note, source = 'user'",
            params![pattern, kind, action, note, now(), mailbox],
        )?;
        self.note_sender_rules()?; // (for settings sync)
        Ok(self.conn.query_row("SELECT id FROM sender_rules WHERE pattern = ?1 AND kind = ?2 AND mailbox = ?3", params![pattern, kind, mailbox], |r| r.get(0))?)
    }

    /// Takes an entry out. An automatic whitelist entry is remembered as taken out, so
    /// rebuilding the whitelist doesn't bring it back.
    pub fn remove_sender_rule(&self, id: i64) -> Result<()> {
        let (pattern, kind, source, mailbox): (String, String, String, String) =
            self.conn.query_row("SELECT pattern, kind, source, mailbox FROM sender_rules WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        self.conn.execute("DELETE FROM sender_rules WHERE id = ?1", [id])?;
        if kind == "allow" && source == "auto" {
            self.conn.execute(
                "INSERT OR IGNORE INTO sender_rules (pattern, kind, action, source, note, added, mailbox) VALUES (?1, 'not-allow', '', 'user', 'taken out of the automatic whitelist', ?2, ?3)",
                params![pattern, now(), mailbox],
            )?;
        }
        self.note_sender_rules()?; // (for settings sync)
        Ok(())
    }

    /// Builds the automatic whitelist from the user's own mail: everyone they sent mail to, and
    /// everyone on a message they replied to or forwarded (its sender and other recipients;
    /// found through their messages' In-Reply-To / References). `mine`: the user's addresses.
    /// Adds what's new; never adds back what the user took out.
    pub fn rebuild_whitelist(&self, mine: &[String]) -> Result<WhitelistReport> {
        let conn = &self.conn;
        conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS my_addresses (address TEXT PRIMARY KEY); DELETE FROM temp.my_addresses;")?;
        for m in mine {
            conn.execute("INSERT OR IGNORE INTO temp.my_addresses VALUES (lower(?1))", params![m])?;
        }
        conn.execute_batch(
            "DROP TABLE IF EXISTS temp.from_me;
             CREATE TEMP TABLE from_me (message INTEGER PRIMARY KEY, mailbox TEXT NOT NULL);
             -- each sent message with the mailbox it's in (its account's address)
             INSERT OR IGNORE INTO temp.from_me
               SELECT ma.message, COALESCE((SELECT lower(ac.address) FROM locations l JOIN folders fo ON fo.id = l.folder JOIN accounts ac ON ac.id = fo.account
                                            WHERE l.message = ma.message LIMIT 1), '')
               FROM message_addresses ma JOIN addresses a ON a.id = ma.address
               WHERE ma.role = 0 AND a.address IN (SELECT address FROM temp.my_addresses);
             DROP TABLE IF EXISTS temp.whitelist_new;
             CREATE TEMP TABLE whitelist_new (address TEXT NOT NULL, why INTEGER NOT NULL, mailbox TEXT NOT NULL);
             -- 1: people the user wrote to (To, Cc, Bcc)
             INSERT INTO temp.whitelist_new
               SELECT DISTINCT a.address, 1, f.mailbox FROM temp.from_me f
               JOIN message_addresses ma ON ma.message = f.message AND ma.role IN (1, 2, 3)
               JOIN addresses a ON a.id = ma.address;
             -- 2: everyone on a message the user replied to or forwarded
             INSERT INTO temp.whitelist_new
               SELECT DISTINCT a.address, 2, f.mailbox FROM temp.from_me f
               JOIN message_refs r ON r.message = f.message
               JOIN messages orig ON orig.message_id = r.ref
               JOIN message_addresses ma ON ma.message = orig.id AND ma.role IN (0, 1, 2)
               JOIN addresses a ON a.id = ma.address;
             DELETE FROM temp.whitelist_new WHERE address NOT LIKE '%_@_%._%' OR address IN (SELECT address FROM temp.my_addresses);",
        )?;
        let count = |why: i64| -> Result<u64> {
            Ok(conn.query_row("SELECT COUNT(DISTINCT address) FROM temp.whitelist_new WHERE why = ?1", [why], |r| r.get::<_, i64>(0))? as u64)
        };
        let (written_to, replied_or_forwarded) = (count(1)?, count(2)?);
        // the automatic whitelist is kept per mailbox: replying to someone from one mailbox
        // doesn't let their mail through in another; entries from before that (every
        // mailbox) go
        conn.execute("DELETE FROM sender_rules WHERE kind = 'allow' AND source = 'auto' AND mailbox = ''", [])?;
        let added = conn.execute(
            "INSERT OR IGNORE INTO sender_rules (pattern, kind, action, source, note, added, mailbox)
             SELECT w.address, 'allow', '', 'auto', CASE MIN(w.why) WHEN 1 THEN 'you wrote to them' ELSE 'on a message you replied to or forwarded' END, ?1, w.mailbox
             FROM temp.whitelist_new w
             WHERE NOT EXISTS (SELECT 1 FROM sender_rules s WHERE s.pattern = w.address AND s.kind IN ('not-allow', 'block') AND s.mailbox IN ('', w.mailbox))
             GROUP BY w.address, w.mailbox",
            [now()],
        )? as u64;
        let total = conn.query_row("SELECT COUNT(*) FROM sender_rules WHERE kind = 'allow' AND source = 'auto'", [], |r| r.get::<_, i64>(0))? as u64;
        Ok(WhitelistReport { written_to, replied_or_forwarded, added, total })
    }

    /// The whitelist and blacklist, ready for checking messages against.
    /// (Only the entries for every mailbox: `mailbox_lists` has each mailbox's own.)
    pub fn sender_lists(&self) -> Result<SenderLists> {
        let mut lists = SenderLists::default();
        for r in self.sender_rules()?.into_iter().filter(|r| r.mailbox.is_empty()) {
            lists.add(r);
        }
        Ok(lists)
    }

    /// Each mailbox's lists (its address → the entries for every mailbox plus its own), for the
    /// mailboxes that have entries of their own.
    pub fn mailbox_lists(&self) -> Result<HashMap<String, SenderLists>> {
        let rules = self.sender_rules()?;
        let mut out: HashMap<String, SenderLists> = HashMap::new();
        for mailbox in rules.iter().filter(|r| !r.mailbox.is_empty()).map(|r| r.mailbox.clone()).collect::<HashSet<_>>() {
            let mut lists = SenderLists::default();
            for r in rules.iter().filter(|r| r.mailbox.is_empty() || r.mailbox == mailbox) {
                lists.add(r.clone());
            }
            out.insert(mailbox, lists);
        }
        Ok(out)
    }
}

/// The whitelist and blacklist in memory.
#[derive(Clone, Debug, Default)]
pub struct SenderLists {
    pub allowed: HashSet<String>,
    pub allowed_domains: Vec<String>,
    /// (pattern, action, entry id); an address entry wins over a domain entry.
    pub blocked: Vec<(String, Action, i64)>,
    /// Whitelist entries the user added (not the automatic ones): their mass mailings stay too.
    pub chosen: Vec<String>,
}

impl SenderLists {
    fn add(&mut self, r: SenderRule) {
        match r.kind.as_str() {
            "allow" => {
                if r.source == "user" {
                    self.chosen.push(r.pattern.clone());
                }
                if r.pattern.starts_with('@') {
                    self.allowed_domains.push(r.pattern);
                } else {
                    self.allowed.insert(r.pattern);
                }
            }
            "block" => self.blocked.push((r.pattern.clone(), r.action.clone().unwrap_or_default(), r.id)),
            _ => {}
        }
    }

    pub fn is_allowed(&self, address: &str) -> bool {
        let a = address.to_lowercase();
        self.allowed.contains(&a) || self.allowed_domains.iter().any(|d| pattern_matches(d, &a))
    }

    /// The whitelist entry `address` is on: the address itself, else the domain entry.
    pub fn allowed_match(&self, address: &str) -> Option<String> {
        let a = address.to_lowercase();
        if self.allowed.contains(&a) {
            return Some(a);
        }
        self.allowed_domains.iter().filter(|d| pattern_matches(d, &a)).max_by_key(|d| d.len()).cloned()
    }

    /// Every blacklist entry for `address`, most specific first (its address, then the
    /// longest domain): one can be switched off or given an exception, the next still applies.
    /// (pattern, action, whether `address` matches it); the matching ones first, the rest
    /// after (an inclusion can bring other senders in).
    pub fn blocked_entries(&self, address: &str) -> Vec<(String, &Action, bool)> {
        let a = address.to_lowercase();
        let mut out: Vec<(String, &Action, bool)> =
            self.blocked.iter().map(|(p, act, _)| (p.clone(), act, !a.is_empty() && pattern_matches(p, &a))).collect();
        out.sort_by_key(|(p, _, hit)| (!hit, p.starts_with('@'), std::cmp::Reverse(p.len())));
        out
    }

    /// On the whitelist because the user put it there.
    pub fn is_chosen(&self, address: &str) -> bool {
        let a = address.to_lowercase();
        self.chosen.iter().any(|p| pattern_matches(p, &a))
    }

    /// The blacklist entry for `address`: its own address entry first, else its domain's.
    pub fn blocked(&self, address: &str) -> Option<(&str, &Action, i64)> {
        let a = address.to_lowercase();
        self.blocked
            .iter()
            .filter(|(p, _, _)| pattern_matches(p, &a))
            .max_by_key(|(p, _, _)| (!p.starts_with('@'), p.len()))
            .map(|(p, action, id)| (p.as_str(), action, *id))
    }
}

// ------------------------------------------------------------------ what a message's headers say

/// What a message's headers say about where it comes from.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Signals {
    pub from: String,
    pub from_name: String,
    /// The From address's domain.
    pub from_domain: String,
    /// List-Id (RFC 2919), without the angle brackets.
    pub list_id: Option<String>,
    /// List-Unsubscribe (RFC 2369) is there.
    pub unsubscribe: bool,
    /// List-Unsubscribe-Post: List-Unsubscribe=One-Click (RFC 8058).
    pub one_click: bool,
    /// The unsubscribe targets (https and mailto).
    pub unsubscribe_targets: Vec<String>,
    /// Precedence: bulk / list / junk.
    pub precedence: Option<String>,
    /// Auto-Submitted other than "no" (RFC 3834: auto-generated, auto-replied).
    pub auto_submitted: Option<String>,
    /// Feedback-ID (Gmail's; `a:b:c:sender`).
    pub feedback_id: Option<String>,
    /// The bulk-mail service the headers name (SendGrid, Mailchimp, Amazon SES…).
    pub esp: Option<String>,
    /// The Return-Path's domain (where bounces go).
    pub return_path_domain: Option<String>,
    /// DKIM signing domains (d=).
    pub dkim_domains: Vec<String>,
    /// From a no-reply / notifications address.
    pub no_reply: bool,
    /// From a system rather than a person (root@, veeam@, backup-…@, alerts@…).
    pub machine: bool,
    /// A subject only a system writes ("[Success] …", "Host … is down"). A person could write
    /// one too, so it counts only for senders who aren't on the whitelist.
    pub machine_subject: bool,
    /// From a marketing address (marketing@, newsletter@, deals@…).
    pub marketer: bool,
    /// Authentication-Results: spf / dkim / dmarc results ("pass", "fail", "softfail", "none"…).
    pub spf: Option<String>,
    pub dkim: Option<String>,
    pub dmarc: Option<String>,
    /// Microsoft 365: spam confidence level (X-MS-Exchange-Organization-SCL; -1 = trusted, 5+ = spam).
    pub scl: Option<i32>,
    /// Microsoft 365: bulk complaint level (BCL in X-Microsoft-Antispam, 0-9).
    pub bcl: Option<i32>,
    /// The character sets the message declares (Content-Type, encoded words in Subject / From),
    /// lowercase: a hint of its language, never proof.
    pub charsets: Vec<String>,
    /// Microsoft 365: spam filtering verdict (SFV in X-Forefront-Antispam-Report: SPM = spam, BLK = blocked sender, SKN/SKA = skipped…).
    pub sfv: Option<String>,
    /// SpamAssassin-style X-Spam-Flag: YES / X-Spam-Status: Yes.
    pub spam_flag: bool,
}

fn domain_of(address: &str) -> String {
    address.rsplit_once('@').map(|(_, d)| d.trim_end_matches('>').to_lowercase()).unwrap_or_default()
}

/// The address inside a header value ("Name <a@b.c>" → a@b.c).
fn address_in(value: &str) -> String {
    let v = value.trim();
    let inner = match (v.rfind('<'), v.rfind('>')) {
        (Some(a), Some(b)) if b > a => &v[a + 1..b],
        _ => v,
    };
    inner.trim().trim_matches(['"', '\'']).to_lowercase()
}

/// `key=value` out of a `;`-separated header part ("dmarc=fail (p=reject…)" → fail).
fn result_of(auth: &str, key: &str) -> Option<String> {
    let lower = auth.to_lowercase();
    let at = lower.find(&format!("{key}="))?;
    Some(lower[at + key.len() + 1..].split(|c: char| !c.is_ascii_alphanumeric()).next()?.to_string())
}

/// Markers the big bulk-mail services leave in their mail: (header name, or a prefix ending in
/// '*'; the service).
const ESP_HEADERS: [(&str, &str); 22] = [
    ("x-sg-eid", "SendGrid"),
    ("x-sg-id", "SendGrid"),
    ("x-mc-user", "Mailchimp"),
    ("x-mandrill-user", "Mailchimp (Mandrill)"),
    ("x-campaignid", "a mailing service"),
    ("x-ses-outgoing", "Amazon SES"),
    ("x-mailgun-*", "Mailgun"),
    ("x-mailgun-sid", "Mailgun"),
    ("x-hs-cid", "HubSpot"),
    ("x-hubspot-*", "HubSpot"),
    ("x-sfmc-stack", "Salesforce Marketing Cloud"),
    ("x-messagesentbyet", "Salesforce Marketing Cloud"),
    ("x-kmail-*", "Klaviyo"),
    ("x-klaviyo-*", "Klaviyo"),
    ("x-roving-id", "Constant Contact"),
    ("x-ctct-*", "Constant Contact"),
    ("x-mkto-*", "Marketo"),
    ("x-marketo-*", "Marketo"),
    ("x-pm-message-id", "Postmark"),
    ("x-sparkpost-*", "SparkPost"),
    ("x-campaign-*", "a mailing service"),
    ("x-mailer-recptid", "a mailing service"),
];

const NO_REPLY: [&str; 11] = ["noreply", "no-reply", "no_reply", "no.reply", "donotreply", "do-not-reply", "do_not_reply", "do.not.reply", "notifications", "notification", "mailer-daemon"];

/// Address parts (before the @) of systems rather than people: servers, backups, monitoring.
/// Whole words of the address part ("root", "veeam-app1", "backup-site2", "alerts_no-reply").
const MACHINES: [&str; 26] = [
    "root", "postmaster", "daemon", "cron", "cpanel", "plesk", "nagios", "zabbix", "prtg", "monitor", "monitoring", "alert", "alerts",
    "alarm", "alarms", "backup", "backups", "veeam", "noc-alerts", "system", "robot", "automated", "automail", "reports", "report", "status",
];

/// Address parts of marketing senders ("marketing@", "newsletter@", "deals@").
const MARKETERS: [&str; 9] = ["marketing", "newsletter", "newsletters", "news", "deals", "offers", "promo", "promotions", "specials"];

/// Whether one of `names` is a word of the address part (split at - _ . + and digits).
fn has_part(local: &str, names: &[&str]) -> bool {
    local.split(|c: char| !c.is_ascii_alphabetic()).any(|w| names.contains(&w)) || names.iter().any(|n| n.contains('-') && local.contains(n))
}

/// Names no person goes by, found anywhere in an address part ("veeamcorp", "nasimgbackup",
/// "monitor-vmhost1").
const MACHINE_PIECES: [&str; 8] = ["veeam", "backup", "vmhost", "nagios", "zabbix", "monitor", "mailer-daemon", "postmaster"];

/// Subjects only a system writes: job results, up / down pings, alarms.
const MACHINE_SUBJECTS: [&str; 14] = [
    "[success]", "[failed]", "[warning]", "[error]", "[critical]", "[ok]", "automated message", " is down", " is up", "alarm",
    "backup job", "weekly backup", "nightly backup", "cron <",
];

/// Whether an address belongs to a system rather than a person.
fn machine_address(local: &str) -> bool {
    has_part(local, &MACHINES) || MACHINE_PIECES.iter().any(|p| local.contains(p))
}

/// Whether a subject reads like a system's ("[Success] …", "Host … is down").
fn machine_subject(subject: &str) -> bool {
    let subject = subject.to_lowercase();
    MACHINE_SUBJECTS.iter().any(|m| subject.contains(m))
}

/// The SPF / DKIM / DMARC verdict to believe. Headers are newest first, and anything below the
/// hop where the mail provider (Microsoft 365, Google) first took the message from the Internet
/// could have been written by the sender, so only results above it count: of those, the one
/// nearest that first arrival, whoever wrote it (the provider, or a security gateway it hands
/// the mail to). Such a gateway (any) takes the mail out and sends
/// it back in changed, and the provider's second look then fails SPF, DKIM and DMARC for nearly
/// everything; the first verdict is kept lower down, often as an ARC header. Mail through
/// neither provider: the topmost Authentication-Results.
pub fn trusted_auth_results(headers: &[(String, String)]) -> Option<&str> {
    for receiver in MAIL_PROVIDERS {
        let first_arrival = headers.iter().rposition(|(n, v)| n == "received" && v.to_lowercase().contains(receiver));
        if let Some(b) = first_arrival {
            let nearest = headers[..b].iter().rev().find(|(n, v)| (n == "authentication-results" || n == "arc-authentication-results") && v.to_lowercase().contains("dmarc="));
            if let Some((_, v)) = nearest {
                return Some(v.as_str());
            }
        }
    }
    headers.iter().find(|(n, _)| n == "authentication-results").map(|(_, v)| v.as_str())
}

/// Mail providers whose first look at a message is the one to believe: what their receiving
/// servers write in `Received: … by …`.
const MAIL_PROVIDERS: [&str; 2] = [".mail.protection.outlook.com", "by mx.google.com"];

/// Reads the signals out of a message's headers (`header_index::read_headers`).
pub fn signals(headers: &[(String, String)]) -> Signals {
    let get = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    let mut s = Signals::default();
    if let Some(from) = get("from") {
        s.from = address_in(from);
        s.from_domain = domain_of(&s.from);
        s.from_name = from.rsplit_once('<').map(|(n, _)| n.trim().trim_matches('"').to_string()).unwrap_or_default();
        let local = s.from.split('@').next().unwrap_or("");
        s.no_reply = NO_REPLY.iter().any(|n| local.contains(n));
        s.machine = machine_address(local);
        s.machine_subject = machine_subject(get("subject").unwrap_or(""));
        s.marketer = has_part(local, &MARKETERS);
    }
    s.list_id = get("list-id").map(|v| address_in(v).trim().to_string()).filter(|v| !v.is_empty());
    if let Some(u) = get("list-unsubscribe") {
        s.unsubscribe = true;
        s.unsubscribe_targets = u.split(',').map(|t| t.trim().trim_matches(['<', '>']).to_string()).filter(|t| t.starts_with("http") || t.starts_with("mailto:")).collect();
    }
    s.one_click = get("list-unsubscribe-post").is_some_and(|v| v.to_lowercase().contains("one-click"));
    s.precedence = get("precedence").map(|v| v.trim().to_lowercase()).filter(|v| ["bulk", "list", "junk"].contains(&v.as_str()));
    s.auto_submitted = get("auto-submitted").map(|v| v.trim().to_lowercase()).filter(|v| v != "no");
    s.feedback_id = get("feedback-id").map(|v| v.trim().to_string());
    s.return_path_domain = get("return-path").map(|v| domain_of(&address_in(v))).filter(|d| !d.is_empty());
    for v in [get("content-type"), get("subject"), get("from")].into_iter().flatten() {
        let lower = v.to_ascii_lowercase();
        let named = lower.split("charset=").skip(1).map(|r| r.trim_start_matches('"').split(['"', ';', ' ']).next().unwrap_or("").to_string());
        let encoded = lower.split("=?").skip(1).map(|r| r.split('?').next().unwrap_or("").to_string());
        for c in named.chain(encoded).filter(|c| !c.is_empty()) {
            if !s.charsets.contains(&c) {
                s.charsets.push(c);
            }
        }
    }
    for (n, v) in headers {
        if n == "dkim-signature" {
            if let Some(d) = v.split(';').find_map(|p| p.trim().strip_prefix("d=")) {
                let d = d.trim().to_lowercase();
                if !s.dkim_domains.contains(&d) {
                    s.dkim_domains.push(d);
                }
            }
        }
        if s.esp.is_none() {
            s.esp = ESP_HEADERS
                .iter()
                .find(|(h, _)| match h.strip_suffix('*') {
                    Some(prefix) => n.starts_with(prefix),
                    None => n == h,
                })
                .map(|(_, esp)| esp.to_string());
        }
    }
    if let Some(auth) = trusted_auth_results(headers) {
        s.spf = result_of(auth, "spf");
        s.dkim = result_of(auth, "dkim");
        s.dmarc = result_of(auth, "dmarc");
    }
    s.scl = get("x-ms-exchange-organization-scl").and_then(|v| v.trim().parse().ok());
    if let Some(ms) = get("x-microsoft-antispam") {
        s.bcl = ms.split(';').find_map(|p| p.trim().strip_prefix("BCL:")).and_then(|v| v.trim().parse().ok());
    }
    if let Some(report) = get("x-forefront-antispam-report") {
        s.sfv = report.split(';').find_map(|p| p.trim().strip_prefix("SFV:")).map(|v| v.trim().to_string());
        if s.scl.is_none() {
            s.scl = report.split(';').find_map(|p| p.trim().strip_prefix("SCL:")).and_then(|v| v.trim().parse().ok());
        }
    }
    s.spam_flag = get("x-spam-flag").is_some_and(|v| v.trim().eq_ignore_ascii_case("yes")) || get("x-spam-status").is_some_and(|v| v.trim().to_lowercase().starts_with("yes"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header_index::read_headers;

    #[test]
    fn the_first_microsoft_verdict_counts_not_the_gateways_second_pass() {
        // newest first: Microsoft's second look (after the gateway changed the message) fails;
        // the gateway's ARC copy of the first verdict passes; a forged "pass" below the first
        // arrival is never believed
        let raw = b"Authentication-Results: mx.microsoft.com 1; spf=fail; dkim=fail; dmarc=fail action=oreject\r\n\
Received: from ec2-192-0-2-10.compute-1.amazonaws.com by BY1.mail.protection.outlook.com\r\n\
Arc-Authentication-Results: i=1; smtp.gateway.example; mx.microsoft.com 1; spf=pass; dkim=pass; dmarc=pass action=none\r\n\
Received: from SN1.outbound.protection.outlook.com by email-smtp-gw\r\n\
Received: from r223.mail.northeast.aaa.com by DM3.mail.protection.outlook.com\r\n\
From: AAA <email@mail.northeast.aaa.com>\r\n\r\nhi";
        let s = signals(&read_headers(raw));
        assert_eq!((s.spf.as_deref(), s.dkim.as_deref(), s.dmarc.as_deref()), (Some("pass"), Some("pass"), Some("pass")));
        let forged = b"Authentication-Results: mx.microsoft.com 1; spf=fail; dmarc=fail\r\n\
Received: from bad.example by DM3.mail.protection.outlook.com\r\n\
Authentication-Results: mx.microsoft.com 1; spf=pass; dmarc=pass\r\n\
From: x@bank.example\r\n\r\nhi";
        assert_eq!(signals(&read_headers(forged)).dmarc.as_deref(), Some("fail"));
    }

    #[test]
    fn patterns() {
        assert_eq!(normalize_pattern(" Bob@Example.COM ").unwrap(), "bob@example.com");
        assert_eq!(normalize_pattern("@Example.com").unwrap(), "@example.com");
        assert!(normalize_pattern("bob").is_err() && normalize_pattern("@com").is_err());
        assert!(pattern_matches("@example.com", "a@mail.example.com") && pattern_matches("@example.com", "a@example.com"));
        assert!(!pattern_matches("@example.com", "a@badexample.com"));
    }

    #[test]
    fn a_newsletters_headers_say_what_it_is() {
        let raw = b"From: \"Acme Deals\" <no-reply@em.acme.example>\r\nList-Id: Acme News <news.acme.example>\r\nList-Unsubscribe: <https://acme.example/u/1>, <mailto:u@acme.example>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nPrecedence: bulk\r\nFeedback-ID: 1:2:campaign9:acme\r\nX-SG-EID: abc\r\nReturn-Path: <bounce-1@sendgrid.example>\r\nDKIM-Signature: v=1; a=rsa-sha256; d=acme.example; s=s1\r\nAuthentication-Results: mx.example; spf=pass smtp.mailfrom=sendgrid.example; dkim=pass header.d=acme.example; dmarc=pass action=none\r\nX-MS-Exchange-Organization-SCL: 1\r\nX-Microsoft-Antispam: BCL:7;\r\nX-Forefront-Antispam-Report: CIP:1.2.3.4;SFV:NSPM;SCL:1\r\n\r\nhi";
        let s = signals(&read_headers(raw));
        assert_eq!((s.from.as_str(), s.from_domain.as_str(), s.from_name.as_str()), ("no-reply@em.acme.example", "em.acme.example", "Acme Deals"));
        assert_eq!(s.list_id.as_deref(), Some("news.acme.example"));
        assert!(s.unsubscribe && s.one_click && s.no_reply);
        assert_eq!(s.unsubscribe_targets, vec!["https://acme.example/u/1", "mailto:u@acme.example"]);
        assert_eq!(s.precedence.as_deref(), Some("bulk"));
        assert_eq!(s.esp.as_deref(), Some("SendGrid"));
        assert_eq!(s.return_path_domain.as_deref(), Some("sendgrid.example"));
        assert_eq!(s.dkim_domains, vec!["acme.example"]);
        assert_eq!((s.spf.as_deref(), s.dkim.as_deref(), s.dmarc.as_deref()), (Some("pass"), Some("pass"), Some("pass")));
        assert_eq!((s.scl, s.bcl, s.sfv.as_deref()), (Some(1), Some(7), Some("NSPM")));
    }

    #[test]
    fn the_whitelist_comes_from_the_users_own_mail() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |name: &str, text: String| (name.to_string(), text);
        let files = [
            // Carol wrote to me (cc Dave); I replied to her: Carol and Dave are whitelisted
            mail("1.eml", "From: carol@x.com\r\nTo: me@example.com\r\nCc: dave@y.com\r\nSubject: hi\r\nMessage-ID: <c1@x>\r\nDate: Fri, 1 Dec 2023 10:00:00 +0000\r\n\r\nhello\r\n".into()),
            mail("2.eml", "From: me@example.com\r\nTo: carol@x.com\r\nSubject: Re: hi\r\nMessage-ID: <m1@example.com>\r\nIn-Reply-To: <c1@x>\r\nReferences: <c1@x>\r\nDate: Fri, 1 Dec 2023 11:00:00 +0000\r\n\r\nthanks\r\n".into()),
            // I wrote to Erin out of the blue
            mail("3.eml", "From: me@example.com\r\nTo: erin@z.com\r\nBcc: fay@z.com\r\nSubject: hello\r\nMessage-ID: <m2@example.com>\r\nDate: Fri, 1 Dec 2023 12:00:00 +0000\r\n\r\nhey\r\n".into()),
            // a newsletter I never answered
            mail("4.eml", "From: news@shop.com\r\nTo: me@example.com\r\nSubject: deals\r\nMessage-ID: <n1@shop.com>\r\nDate: Fri, 1 Dec 2023 13:00:00 +0000\r\n\r\nbuy\r\n".into()),
        ];
        let mut paths = Vec::new();
        for (name, text) in &files {
            std::fs::write(src.join(name), text).unwrap();
            paths.push(src.join(name));
        }
        crate::import(&mut library, &paths, &crate::ImportOptions::default(), &Silent).unwrap();
        let report = library.rebuild_whitelist(&["me@example.com".into()]).unwrap();
        assert_eq!((report.written_to, report.replied_or_forwarded, report.total), (3, 2, 4), "{report:?}");
        let lists = library.sender_lists().unwrap();
        for who in ["carol@x.com", "dave@y.com", "erin@z.com", "fay@z.com"] {
            assert!(lists.is_allowed(who), "{who}");
        }
        assert!(!lists.is_allowed("news@shop.com") && !lists.is_allowed("me@example.com"));
        // taking someone out of the automatic whitelist sticks
        let dave = library.sender_rules().unwrap().into_iter().find(|r| r.pattern == "dave@y.com").unwrap();
        library.remove_sender_rule(dave.id).unwrap();
        library.rebuild_whitelist(&["me@example.com".into()]).unwrap();
        assert!(!library.sender_lists().unwrap().is_allowed("dave@y.com"));
        // the blacklist: each entry its own action; an address entry wins over its domain's
        library.set_sender_rule("@shop.com", "block", Some(&Action::Trash), "").unwrap();
        library.set_sender_rule("news@shop.com", "block", None, "newsletter").unwrap();
        let lists = library.sender_lists().unwrap();
        assert_eq!(lists.blocked("news@shop.com").unwrap().1, &Action::Move { folder: IGNORED_FOLDER.into() });
        assert_eq!(lists.blocked("sales@mail.shop.com").unwrap().1, &Action::Trash);
        assert!(lists.blocked("carol@x.com").is_none());
        // whitelisting takes them off the blacklist
        library.set_sender_rule("news@shop.com", "allow", None, "").unwrap();
        let lists = library.sender_lists().unwrap();
        assert!(lists.is_allowed("news@shop.com"));
        assert_eq!(lists.blocked("news@shop.com").unwrap().0, "@shop.com", "the domain entry still applies to others");
    }

    #[test]
    fn each_mailbox_has_its_own_lists() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let files = [
            ("1.eml", "From: me@example.com\r\nTo: carol@x.com\r\nSubject: hi\r\nMessage-ID: <m1@example.com>\r\nDate: Fri, 1 Dec 2023 11:00:00 +0000\r\n\r\nhi\r\n"),
            ("2.eml", "From: me@example.com\r\nTo: erin@z.com\r\nSubject: hello\r\nMessage-ID: <m2@example.com>\r\nDate: Fri, 1 Dec 2023 12:00:00 +0000\r\n\r\nhey\r\n"),
        ];
        let mut paths = Vec::new();
        for (name, text) in files {
            std::fs::write(src.join(name), text).unwrap();
            paths.push(src.join(name));
        }
        crate::import(&mut library, &paths, &crate::ImportOptions::default(), &Silent).unwrap();
        // message 1 is in mailbox me@example.com, message 2 in shared@example.com
        let conn = &library.conn;
        conn.execute("UPDATE accounts SET address = 'Me@example.com'", []).unwrap();
        conn.execute("INSERT INTO accounts (kind, name, address, position, created) VALUES ('graph-shared', 'Shared', 'shared@example.com', 9, 0)", []).unwrap();
        let shared = conn.last_insert_rowid();
        let folder = crate::db::ensure_folder(conn, shared, "Sent Items").unwrap();
        let id2: i64 = conn.query_row("SELECT id FROM messages WHERE message_id LIKE '%m2@example.com%'", [], |r| r.get(0)).unwrap();
        conn.execute("UPDATE locations SET folder = ?1 WHERE message = ?2", params![folder, id2]).unwrap();
        library.rebuild_whitelist(&["me@example.com".into()]).unwrap();
        let by_mailbox = library.mailbox_lists().unwrap();
        let (mine, theirs) = (&by_mailbox["me@example.com"], &by_mailbox["shared@example.com"]);
        assert!(mine.is_allowed("carol@x.com") && !mine.is_allowed("erin@z.com"));
        assert!(theirs.is_allowed("erin@z.com") && !theirs.is_allowed("carol@x.com"));
        assert!(!library.sender_lists().unwrap().is_allowed("carol@x.com"), "not in every mailbox");
        // a blacklist entry in one mailbox only; one for every mailbox shows in both
        library.set_sender_rule_in("@spam.example", "block", None, "", "shared@example.com").unwrap();
        library.set_sender_rule("@junk.example", "block", None, "").unwrap();
        let by_mailbox = library.mailbox_lists().unwrap();
        assert!(by_mailbox["shared@example.com"].blocked("a@spam.example").is_some());
        assert!(by_mailbox["me@example.com"].blocked("a@spam.example").is_none());
        assert!(by_mailbox["me@example.com"].blocked("a@junk.example").is_some() && by_mailbox["shared@example.com"].blocked("a@junk.example").is_some());
        // whitelisting in a mailbox leaves the other mailboxes' entries alone
        library.set_sender_rule_in("a@spam.example", "allow", None, "", "me@example.com").unwrap();
        assert!(library.mailbox_lists().unwrap()["shared@example.com"].blocked("a@spam.example").is_some());
        // taken out of one mailbox's automatic whitelist: stays out there, stays in the other
        let carol = library.sender_rules().unwrap().into_iter().find(|r| r.pattern == "carol@x.com").unwrap();
        assert_eq!(carol.mailbox, "me@example.com");
        library.remove_sender_rule(carol.id).unwrap();
        library.rebuild_whitelist(&["me@example.com".into()]).unwrap();
        assert!(!library.mailbox_lists().unwrap()["me@example.com"].is_allowed("carol@x.com"));
    }
}
