//! The spam filter's learning (the project's design notes step 4): Robinson–Fisher chi-squared over the
//! words, word pairs and header facts of a message, **one model per mailbox**. Trained from
//! what each mailbox already says: its Junk / Spam folders are spam (never mail from people the
//! user corresponds with), mail from people the user corresponds with and mail they replied to
//! is good; then every Spam / Not Spam the user marks trains it at once. Its verdict is one
//! more rule of the spam filter ("Learned").
//!
//! Scores: under 0.20 good, over 0.90 spam, between: **Maybe** (the user's corrections there
//! teach it the most).

use std::collections::HashMap;

use rusqlite::params;

use crate::error::Result;
use crate::library::Library;
use crate::rules::Facts;

/// Robinson's strength of the background belief, and that belief.
const S: f64 = 0.45;
const X: f64 = 0.5;
/// Tokens that say little either way are left out; at most this many count.
const MIN_STRENGTH: f64 = 0.1;
const MAX_CLUES: usize = 150;
const MAX_TOKENS: usize = 3000;
/// Below: good; above: spam; between: Maybe.
pub const HAM_BELOW: f64 = 0.20;
pub const SPAM_ABOVE: f64 = 0.90;
/// A model needs this many examples of each kind before it says anything.
pub const MIN_EXAMPLES: u64 = 50;
/// The most examples of each kind taken when a mailbox is first trained.
pub const BOOTSTRAP: usize = 50_000;

fn hash(token: &str) -> i64 {
    let h = blake3::hash(token.as_bytes());
    i64::from_be_bytes(h.as_bytes()[..8].try_into().unwrap())
}

/// A message's tokens (hashed, each once): words of 3-20 letters and word pairs (next and one
/// apart) from the subject and text, and header facts: sender address, name words and
/// organization, Reply-To, List-Id, the bounce domain, DKIM signers, authentication results,
/// Microsoft's verdicts, and the hosts of links.
pub fn tokens(facts: &Facts) -> Vec<i64> {
    let header = |name: &str| facts.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str()).unwrap_or("");
    let mut out: Vec<String> = Vec::new();
    let words = |text: &str| -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '$')
            .map(|w| w.trim_matches('\'').to_lowercase())
            .filter(|w| (3..=20).contains(&w.chars().count()))
            .collect()
    };
    for w in words(header("subject")) {
        out.push(format!("s:{w}"));
    }
    let body: String = facts.body.chars().take(20_000).collect();
    let body_words = words(&body);
    for (i, w) in body_words.iter().enumerate() {
        out.push(w.clone());
        if let Some(next) = body_words.get(i + 1) {
            out.push(format!("{w} {next}"));
        }
        if let Some(skip) = body_words.get(i + 2) {
            out.push(format!("{w} _ {skip}"));
        }
    }
    let s = crate::triage::signals(&facts.headers);
    if !s.from.is_empty() {
        out.push(format!("from:{}", s.from.to_lowercase()));
        out.push(format!("fromorg:{}", crate::triage_decide::org_domain(&s.from_domain)));
    }
    for w in words(&s.from_name) {
        out.push(format!("name:{w}"));
    }
    let reply_to = header("reply-to");
    if !reply_to.is_empty() {
        out.push(format!("replyto:{}", reply_to.to_lowercase()));
    }
    if let Some(l) = &s.list_id {
        out.push(format!("list:{l}"));
    }
    if let Some(rp) = &s.return_path_domain {
        out.push(format!("rp:{}", crate::triage_decide::org_domain(rp)));
    }
    for d in &s.dkim_domains {
        out.push(format!("dkim:{}", crate::triage_decide::org_domain(d)));
    }
    for (k, v) in [("spf", &s.spf), ("dkim", &s.dkim), ("dmarc", &s.dmarc)] {
        out.push(format!("auth:{k}={}", v.as_deref().unwrap_or("none")));
    }
    if let Some(e) = &s.esp {
        out.push(format!("esp:{e}"));
    }
    if let Some(scl) = s.scl {
        out.push(format!("scl:{scl}"));
    }
    out.push(format!("unsub:{}", s.unsubscribe));
    // the hosts of links in the text
    for part in body.split("http").skip(1) {
        let rest = part.trim_start_matches('s').trim_start_matches("://");
        let host: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '.' || *c == '-').collect();
        if host.contains('.') {
            out.push(format!("url:{}", crate::triage_decide::org_domain(&host)));
        }
    }
    let mut hashed: Vec<i64> = out.iter().map(|t| hash(t)).collect();
    hashed.sort_unstable();
    hashed.dedup();
    hashed.truncate(MAX_TOKENS);
    hashed
}

/// The chi-squared survival function for an even number of degrees of freedom.
fn chi2q(x2: f64, v: usize) -> f64 {
    let m = x2 / 2.0;
    let mut term = (-m).exp();
    let mut sum = term;
    for i in 1..v / 2 {
        term *= m / i as f64;
        sum += term;
    }
    sum.min(1.0)
}

/// A mailbox's model: how often each token was in its spam and its good mail.
#[derive(Default)]
pub struct Model {
    pub spam_messages: u64,
    pub ham_messages: u64,
    counts: HashMap<i64, (u32, u32)>,
}

impl Model {
    /// Whether it has seen enough to say anything.
    pub fn ready(&self) -> bool {
        self.spam_messages >= MIN_EXAMPLES && self.ham_messages >= MIN_EXAMPLES
    }

    /// How spammy a message is (0 = good … 1 = spam), from its tokens.
    pub fn score(&self, tokens: &[i64]) -> f64 {
        let (ns, nh) = (self.spam_messages.max(1) as f64, self.ham_messages.max(1) as f64);
        let mut clues: Vec<f64> = tokens
            .iter()
            .filter_map(|t| self.counts.get(t))
            .map(|(spam, ham)| {
                let (b, g) = (*spam as f64 / ns, *ham as f64 / nh);
                let p = if b + g == 0.0 { X } else { b / (b + g) };
                let n = (*spam + *ham) as f64;
                (S * X + n * p) / (S + n)
            })
            .filter(|f| (f - 0.5).abs() >= MIN_STRENGTH)
            .collect();
        if clues.is_empty() {
            return 0.5;
        }
        clues.sort_by(|a, b| (b - 0.5).abs().total_cmp(&(a - 0.5).abs()));
        clues.truncate(MAX_CLUES);
        let n = clues.len();
        let spam_evidence = chi2q(-2.0 * clues.iter().map(|f| f.max(1e-200).ln()).sum::<f64>(), 2 * n);
        let ham_evidence = chi2q(-2.0 * clues.iter().map(|f| (1.0 - f).max(1e-200).ln()).sum::<f64>(), 2 * n);
        (1.0 + spam_evidence - ham_evidence) / 2.0
    }
}

/// What training did.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Trained {
    pub spam: u64,
    pub ham: u64,
}

impl Library {
    /// How many spam and good messages a mailbox has learned (without reading its model).
    pub fn bayes_counts(&self, account: i64) -> Result<(u64, u64)> {
        let (spam, ham): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(label = 1), 0), COALESCE(SUM(label = 0), 0) FROM bayes_trained WHERE account = ?1",
            params![account],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((spam as u64, ham as u64))
    }

    /// A mailbox's model (its account id), read from the database.
    pub fn bayes_model(&self, account: i64) -> Result<Model> {
        let mut m = Model::default();
        let (spam, ham): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(label = 1), 0), COALESCE(SUM(label = 0), 0) FROM bayes_trained WHERE account = ?1",
            params![account],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        (m.spam_messages, m.ham_messages) = (spam as u64, ham as u64);
        let mut stmt = self.conn.prepare("SELECT token, spam, ham FROM bayes_tokens WHERE account = ?1")?;
        for row in stmt.query_map(params![account], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))? {
            let (t, s, h) = row?;
            m.counts.insert(t, (s as u32, h as u32));
        }
        Ok(m)
    }

    /// Learns messages as spam (`spam` true) or good, in the account's model; a message
    /// learned the other way before is unlearned first (a correction), one learned the same
    /// way is left. `items`: (message, account). Returns how many were learned.
    pub fn bayes_learn(&self, items: &[(i64, i64)], spam: bool) -> Result<u64> {
        let label = spam as i64;
        let mut todo: Vec<(i64, i64, Option<i64>)> = Vec::new();
        for (message, account) in items {
            let before: Option<i64> = rusqlite::OptionalExtension::optional(self.conn.query_row(
                "SELECT label FROM bayes_trained WHERE message = ?1 AND account = ?2",
                params![message, account],
                |r| r.get(0),
            ))?;
            if before != Some(label) {
                todo.push((*message, *account, before));
            }
        }
        if todo.is_empty() {
            return Ok(0);
        }
        // where each message is (the database), then its tokens (the packs, in parallel)
        let mut stmt = self.conn.prepare_cached("SELECT pack, pack_offset, pack_len FROM messages WHERE id = ?1")?;
        let mut located = Vec::with_capacity(todo.len());
        for (m, a, before) in todo {
            if let Ok(location) = stmt.query_row(params![m], |r| Library::location(r, 0)) {
                located.push((m, a, before, location));
            }
        }
        use rayon::prelude::*;
        let packs = &self.packs;
        let raws: Vec<(i64, i64, Option<i64>, Vec<i64>)> = located
            .par_iter()
            .filter_map(|(m, a, before, location)| {
                let raw = packs.read(*location).ok()?.raw;
                Some((*m, *a, *before, tokens(&Facts::of(&raw, ""))))
            })
            .collect();
        // added up per (account, token), then written once each
        let mut delta: HashMap<(i64, i64), (i64, i64)> = HashMap::new();
        for (_, account, before, toks) in &raws {
            for t in toks {
                let d = delta.entry((*account, *t)).or_default();
                match before {
                    Some(1) => d.0 -= 1,
                    Some(_) => d.1 -= 1,
                    None => {}
                }
                if spam { d.0 += 1 } else { d.1 += 1 }
            }
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let mut up = self.conn.prepare_cached(
                "INSERT INTO bayes_tokens (account, token, spam, ham) VALUES (?1, ?2, MAX(?3, 0), MAX(?4, 0))
                 ON CONFLICT (account, token) DO UPDATE SET spam = MAX(spam + ?3, 0), ham = MAX(ham + ?4, 0)",
            )?;
            for ((account, token), (s, h)) in &delta {
                up.execute(params![account, token, s, h])?;
            }
            let mut mark = self.conn.prepare_cached("INSERT OR REPLACE INTO bayes_trained (message, account, label) VALUES (?1, ?2, ?3)")?;
            for (m, a, _, _) in &raws {
                mark.execute(params![m, a, label])?;
            }
            Ok(())
        })();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result?;
        Ok(raws.len() as u64)
    }

    /// First training of a mailbox from what it already holds: spam = its Junk / Spam folders
    /// (not from people the user corresponds with); good = mail from people the user
    /// corresponds with (and isn't in those folders). The newest `BOOTSTRAP` of each.
    pub fn bayes_bootstrap(&self, account: i64, progress: &dyn crate::Progress) -> Result<Trained> {
        let lists = self.sender_lists()?;
        let junk_folders: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM folders WHERE account = ?1 AND (role = 'junk' OR lower(path) IN ('junk email', 'junk', 'spam', 'junk e-mail', '[gmail]/spam') OR lower(path) LIKE 'junk email/%' OR lower(path) LIKE 'spam/%')",
            )?;
            stmt.query_map(params![account], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let list = |ids: &[i64]| ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let from_of = |m: i64| -> String {
            self.conn
                .query_row(
                    "SELECT a.address FROM message_addresses ma JOIN addresses a ON a.id = ma.address WHERE ma.message = ?1 AND ma.role = 0 ORDER BY ma.position LIMIT 1",
                    params![m],
                    |r| r.get(0),
                )
                .unwrap_or_default()
        };
        let mut spam_ids: Vec<i64> = Vec::new();
        if !junk_folders.is_empty() {
            let mut stmt = self.conn.prepare(&format!("SELECT DISTINCT message FROM locations WHERE folder IN ({}) ORDER BY date DESC LIMIT ?1", list(&junk_folders)))?;
            for id in stmt.query_map(params![(BOOTSTRAP * 2) as i64], |r| r.get::<_, i64>(0))? {
                let id = id?;
                if !lists.is_allowed(&from_of(id)) {
                    spam_ids.push(id);
                }
                if spam_ids.len() >= BOOTSTRAP {
                    break;
                }
            }
        }
        progress.report(0, 0, &format!("Learning from {} messages in Junk / Spam…", spam_ids.len()));
        let mut ham_ids: Vec<i64> = Vec::new();
        let own: Vec<String> = self.own_addresses()?.into_iter().map(|a| a.to_lowercase()).collect();
        let not_junk = if junk_folders.is_empty() { String::new() } else { format!(" AND l.folder NOT IN ({})", list(&junk_folders)) };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT l.message FROM locations l JOIN folders f ON f.id = l.folder WHERE f.account = ?1{not_junk} ORDER BY l.date DESC LIMIT ?2"
        ))?;
        for id in stmt.query_map(params![account, (BOOTSTRAP * 6) as i64], |r| r.get::<_, i64>(0))? {
            let id = id?;
            let from = from_of(id);
            if !from.is_empty() && lists.is_allowed(&from) && !own.contains(&from.to_lowercase()) {
                ham_ids.push(id);
            }
            if ham_ids.len() >= BOOTSTRAP {
                break;
            }
        }
        progress.report(0, 0, &format!("Learning from {} messages from people you write to…", ham_ids.len()));
        let mut done = Trained::default();
        for chunk in spam_ids.chunks(5000) {
            done.spam += self.bayes_learn(&chunk.iter().map(|m| (*m, account)).collect::<Vec<_>>(), true)?;
            progress.report(done.spam, spam_ids.len() as u64, &format!("Learning spam: {} of {}", done.spam, spam_ids.len()));
        }
        for chunk in ham_ids.chunks(5000) {
            done.ham += self.bayes_learn(&chunk.iter().map(|m| (*m, account)).collect::<Vec<_>>(), false)?;
            progress.report(done.ham, ham_ids.len() as u64, &format!("Learning good mail: {} of {}", done.ham, ham_ids.len()));
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spam_words_score_high_and_good_ones_low() {
        let facts = |from: &str, subject: &str, body: &str| Facts::of(format!("From: {from}\r\nSubject: {subject}\r\n\r\n{body}\r\n").as_bytes(), "");
        let mut m = Model::default();
        // train: 60 spams about pills and prizes, 60 good about the project
        for i in 0..60 {
            for t in tokens(&facts(&format!("x{i}@cheap.example"), "Cheap pills prize winner", "claim your prize now, cheap pills, click http://win.example")) {
                m.counts.entry(t).or_default().0 += 1;
            }
            for t in tokens(&facts("ann@work.example", "Project schedule", "the project meeting moved to Tuesday, see the schedule")) {
                m.counts.entry(t).or_default().1 += 1;
            }
        }
        (m.spam_messages, m.ham_messages) = (60, 60);
        assert!(m.ready());
        let spam = m.score(&tokens(&facts("y@cheap.example", "You are a prize winner", "claim your prize: cheap pills")));
        let ham = m.score(&tokens(&facts("ann@work.example", "Schedule", "project meeting Tuesday")));
        let unknown = m.score(&tokens(&facts("z@else.example", "Hello", "lorem ipsum dolor")));
        assert!(spam > SPAM_ABOVE, "{spam}");
        assert!(ham < HAM_BELOW, "{ham}");
        assert!((unknown - 0.5).abs() < 0.01, "nothing known: undecided ({unknown})");
        assert!((chi2q(0.0, 4) - 1.0).abs() < 1e-9 && chi2q(1000.0, 4) < 1e-9);
    }
}
