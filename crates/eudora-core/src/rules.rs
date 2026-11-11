//! Rules (classic Eudora's filters): tests on a message, and what to do when they match.
//!
//! Classic Eudora's language is kept whole (its `Filters.pce`, read by `import_classic`):
//! tests contains / !contains / is / !is / starts / ends / appears / !appears / regex /
//! regex_icase / greater / less on a header (From:, To:, Subject:, any header by name),
//! «Any Header», «Any Recipient», «Body» or «Personality»; two tests joined by and / or /
//! unless; actions transfer / copy / label / priority / status / junk / subject / forward /
//! redirect / reply / notify / stop. Eudora 27 allows any number of tests (all of them / any
//! of them, plus "unless" tests).

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub name: String,
    pub enabled: bool,
    /// When it runs by itself: on mail arriving, on mail sent; and by hand (Filter Messages).
    pub incoming: bool,
    pub outgoing: bool,
    pub manual: bool,
    /// How the tests combine: true = every test, false = any test. Tests marked `unless`
    /// stop the rule when they match.
    pub all: bool,
    pub tests: Vec<Test>,
    pub actions: Vec<Action>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Test {
    /// "From:", "To:", "Subject:", any header name with its colon, or one of
    /// "«Any Header»", "«Any Recipient»", "«Body»", "«Personality»".
    pub field: String,
    pub verb: Verb,
    pub value: String,
    /// Classic "unless": the rule doesn't apply when this matches.
    pub unless: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    #[default]
    Contains,
    NotContains,
    Is,
    IsNot,
    StartsWith,
    EndsWith,
    /// The header is there at all (any value).
    Appears,
    NotAppears,
    Regex,
    RegexIgnoreCase,
    Greater,
    Less,
}

impl Verb {
    /// Classic's word for it in Filters.pce.
    pub fn classic(word: &str) -> Option<Verb> {
        Some(match word {
            "contains" => Verb::Contains,
            "!contains" => Verb::NotContains,
            "is" => Verb::Is,
            "!is" => Verb::IsNot,
            "starts" => Verb::StartsWith,
            "ends" => Verb::EndsWith,
            "appears" => Verb::Appears,
            "!appears" => Verb::NotAppears,
            "regex" => Verb::Regex,
            "regex_icase" => Verb::RegexIgnoreCase,
            "greater" => Verb::Greater,
            "less" => Verb::Less,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Action {
    /// Move to a folder: the account (address, None = the message's own account) and the path.
    Transfer { account: Option<String>, folder: String },
    Copy { account: Option<String>, folder: String },
    Label { label: String },
    /// 1 (highest) … 5 (lowest); or raise / lower by one.
    Priority { level: i8 },
    Status { status: String },
    Junk,
    Delete,
    /// Change the subject ("^" stands for the old one, as classic).
    Subject { text: String },
    Forward { to: String },
    Redirect { to: String },
    Reply { with: String },
    Notify,
    /// Runs a plugin's rule action on the message (it sees only what the user granted it).
    Plugin { plugin: String, command: String },
    /// Sort it with the spam filter (`triage_decide`): Junk, Bulk, Lists… or it stays.
    Sort,
    /// Skip the rules after this one.
    Stop,
}

/// What a rule can see of a message.
pub struct Facts {
    /// Every header (name lowercase, value unfolded), in order.
    pub headers: Vec<(String, String)>,
    /// Every recipient address (To, Cc, Bcc).
    pub recipients: Vec<String>,
    pub body: String,
    /// The account / identity it came to (classic «Personality»).
    pub personality: String,
}

impl Facts {
    /// Reads a stored message's headers and text.
    pub fn of(raw: &[u8], personality: &str) -> Facts {
        let headers = crate::header_index::read_headers(raw);
        let parsed = crate::parse::parse(raw, Default::default(), crate::library::now());
        let recipients = parsed.to.iter().chain(&parsed.cc).chain(&parsed.bcc).map(|a| a.address.clone()).collect();
        Facts { headers, recipients, body: parsed.body_text, personality: personality.to_string() }
    }
}

impl Test {
    fn texts<'a>(&self, f: &'a Facts) -> Vec<&'a str> {
        match self.field.as_str() {
            "«Any Header»" => f.headers.iter().map(|(_, v)| v.as_str()).collect(),
            "«Any Recipient»" => f.recipients.iter().map(String::as_str).collect(),
            "«Body»" => vec![f.body.as_str()],
            "«Personality»" => vec![f.personality.as_str()],
            name => {
                let name = name.trim().trim_end_matches(':').to_ascii_lowercase();
                f.headers.iter().filter(|(n, _)| *n == name).map(|(_, v)| v.as_str()).collect()
            }
        }
    }

    pub fn matches(&self, f: &Facts) -> bool {
        let texts = self.texts(f);
        let value = self.value.to_lowercase();
        let one = |original: &str| -> bool {
            let t = original.to_lowercase();
            match self.verb {
                Verb::Contains | Verb::NotContains => t.contains(&value),
                Verb::Is | Verb::IsNot => t.trim() == value.trim(),
                Verb::StartsWith => t.trim_start().starts_with(value.trim()),
                Verb::EndsWith => t.trim_end().ends_with(value.trim()),
                Verb::Appears | Verb::NotAppears => true,
                Verb::Regex => simple_regex(&self.value, original, false),
                Verb::RegexIgnoreCase => simple_regex(&self.value, original, true),
                Verb::Greater | Verb::Less => {
                    let (a, b) = (t.trim().parse::<f64>(), self.value.trim().parse::<f64>());
                    matches!((a, b), (Ok(a), Ok(b)) if if self.verb == Verb::Greater { a > b } else { a < b })
                }
            }
        };
        let any = texts.iter().any(|t| one(t));
        match self.verb {
            Verb::NotContains | Verb::IsNot | Verb::NotAppears => !any,
            _ => any,
        }
    }
}

/// Classic Eudora's regular expressions were the simple kind: `.` `*` `+` `?` `^` `$` `[…]`
/// and `\`. This matches those (enough for rules; no extra library).
fn simple_regex(pattern: &str, text: &str, ignore_case: bool) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = if ignore_case {
        (pattern.to_lowercase().chars().collect(), text.to_lowercase().chars().collect())
    } else {
        (pattern.chars().collect(), text.chars().collect())
    };
    if p.first() == Some(&'^') {
        return match_here(&p[1..], &t);
    }
    (0..=t.len()).any(|i| match_here(&p, &t[i..]))
}

/// One pattern item: its length in the pattern and whether it matches a character.
fn item(p: &[char]) -> (usize, Box<dyn Fn(char) -> bool + '_>) {
    match p[0] {
        '.' => (1, Box::new(|_| true)),
        '\\' if p.len() > 1 => {
            let c = p[1];
            (2, Box::new(move |x| match c {
                'd' => x.is_ascii_digit(),
                'w' => x.is_alphanumeric() || x == '_',
                's' => x.is_whitespace(),
                _ => x == c,
            }))
        }
        '[' => {
            let end = p.iter().position(|&c| c == ']').filter(|&e| e > 1).unwrap_or(p.len() - 1);
            let set = &p[1..end];
            let (negate, set) = if set.first() == Some(&'^') { (true, &set[1..]) } else { (false, set) };
            (end + 1, Box::new(move |x| {
                let mut i = 0;
                let mut hit = false;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        hit |= set[i] <= x && x <= set[i + 2];
                        i += 3;
                    } else {
                        hit |= set[i] == x;
                        i += 1;
                    }
                }
                hit != negate
            }))
        }
        c => (1, Box::new(move |x| x == c)),
    }
}

fn match_here(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return true;
    }
    if p == ['$'] {
        return t.is_empty();
    }
    let (len, test) = item(p);
    let rest = &p[len..];
    match rest.first() {
        Some('*') | Some('+') | Some('?') => {
            let op = rest[0];
            let rest = &rest[1..];
            let min = if op == '+' { 1 } else { 0 };
            let max = if op == '?' { 1 } else { usize::MAX };
            let mut n = 0;
            while n < max && n < t.len() && test(t[n]) {
                n += 1;
            }
            (min..=n).rev().any(|k| match_here(rest, &t[k..]))
        }
        _ => !t.is_empty() && test(t[0]) && match_here(rest, &t[1..]),
    }
}

impl Rule {
    /// Whether the rule applies to the message.
    pub fn matches(&self, f: &Facts) -> bool {
        let (unless, tests): (Vec<&Test>, Vec<&Test>) = self.tests.iter().partition(|t| t.unless);
        if tests.is_empty() {
            // no tests = nothing, except a spam-filter rule: "sort everything that arrives"
            // (the filter itself decides; a classic rule with blank tests never acted)
            return self.actions.contains(&Action::Sort) && !unless.iter().any(|t| t.matches(f));
        }
        let hit = if self.all { tests.iter().all(|t| t.matches(f)) } else { tests.iter().any(|t| t.matches(f)) };
        hit && !unless.iter().any(|t| t.matches(f))
    }
}

/// The first rule's actions that apply, rule by rule, until a rule says Stop.
pub fn actions_for<'a>(rules: &'a [Rule], f: &Facts, manual: bool, incoming: bool) -> Vec<(&'a Rule, &'a Action)> {
    let mut out = Vec::new();
    // neither by hand nor arriving: mail the user sent (classic's "outgoing" filters)
    let outgoing = !manual && !incoming;
    for rule in rules.iter().filter(|r| r.enabled && ((manual && r.manual) || (incoming && r.incoming) || (outgoing && r.outgoing))) {
        if !rule.matches(f) {
            continue;
        }
        let mut stop = false;
        for a in &rule.actions {
            if *a == Action::Stop {
                stop = true;
            } else {
                out.push((rule, a));
            }
        }
        if stop {
            break;
        }
    }
    out
}

/// Classic Eudora's `Filters.pce` (Windows; version 3) as rules. `account`: the address of the
/// account the classic mailboxes were imported into (its transfer targets are folders there:
/// "BUSINESS.FOL\Card Settlements.mbx" → "BUSINESS.FOL/Card Settlements").
pub fn import_classic(text: &str, account: &str) -> Result<Vec<Rule>> {
    let mut rules: Vec<Rule> = Vec::new();
    let mut lines = text.lines().map(|l| l.trim_end_matches('\r'));
    let first = lines.next().unwrap_or("").trim();
    if !first.chars().all(|c| c.is_ascii_digit()) || first.is_empty() {
        return Err(Error::Other("this isn't a classic Eudora filters file (Filters.pce)".into()));
    }
    // two classic tests, then the conjunction between them, are read before the rule is closed
    let mut pending: Vec<(String, String, String)> = Vec::new();
    let mut conjunction = String::from("ignore");
    let finish = |rule: &mut Rule, pending: &mut Vec<(String, String, String)>, conjunction: &str| {
        for (k, (field, verb, value)) in pending.drain(..).enumerate() {
            if field.is_empty() || (k == 1 && conjunction == "ignore") {
                continue;
            }
            rule.tests.push(Test { field, verb: Verb::classic(&verb).unwrap_or_default(), value, unless: k == 1 && conjunction == "unless" });
        }
        rule.all = conjunction != "or";
    };
    for line in lines {
        let (word, rest) = line.split_once(' ').map(|(w, r)| (w, r.to_string())).unwrap_or((line, String::new()));
        match word {
            "rule" => {
                if let Some(last) = rules.last_mut() {
                    finish(last, &mut pending, &conjunction);
                }
                conjunction = "ignore".into();
                rules.push(Rule { name: rest.trim().to_string(), enabled: true, ..Default::default() });
            }
            _ if rules.is_empty() => {}
            "incoming" => rules.last_mut().unwrap().incoming = true,
            "outgoing" => rules.last_mut().unwrap().outgoing = true,
            "manual" => rules.last_mut().unwrap().manual = true,
            "header" => pending.push((rest.trim().to_string(), String::new(), String::new())),
            "verb" => {
                if let Some(p) = pending.last_mut() {
                    p.1 = rest.trim().to_string();
                }
            }
            "value" => {
                if let Some(p) = pending.last_mut() {
                    p.2 = rest.trim().to_string();
                }
            }
            "conjunction" => conjunction = rest.trim().to_string(),
            _ => {
                if let Some(a) = classic_action(word, rest.trim(), account) {
                    rules.last_mut().unwrap().actions.push(a);
                }
            }
        }
    }
    if let Some(last) = rules.last_mut() {
        finish(last, &mut pending, &conjunction);
    }
    Ok(rules)
}

/// A classic mailbox path ("BUSINESS.FOL\Card Settlements.mbx") as a folder path.
fn classic_folder(path: &str) -> String {
    path.split(['\\', '/']).map(|p| p.strip_suffix(".mbx").or_else(|| p.strip_suffix(".MBX")).unwrap_or(p)).collect::<Vec<_>>().join("/")
}

fn classic_action(word: &str, rest: &str, account: &str) -> Option<Action> {
    let account = Some(account.to_string());
    Some(match word {
        "transfer" => Action::Transfer { account, folder: classic_folder(rest) },
        "copy" => Action::Copy { account, folder: classic_folder(rest) },
        "label" => Action::Label { label: rest.to_string() },
        "priority" => Action::Priority { level: rest.parse().unwrap_or(3) },
        "raise" => Action::Priority { level: -1 },
        "lower" => Action::Priority { level: -2 },
        "status" => Action::Status { status: rest.to_string() },
        "junk" => Action::Junk,
        "subject" => Action::Subject { text: rest.to_string() },
        "forward" => Action::Forward { to: rest.to_string() },
        "redirect" => Action::Redirect { to: rest.to_string() },
        "reply" => Action::Reply { with: rest.to_string() },
        "notifyUser" | "notifyApp" => Action::Notify,
        "stop" => Action::Stop,
        _ => return None,
    })
}

/// What running the rules did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RunReport {
    pub checked: u64,
    pub matched: u64,
    pub moved: u64,
    pub copied: u64,
    pub deleted: u64,
    /// (rule, how many messages it matched)
    pub by_rule: Vec<(String, u64)>,
    /// Actions that couldn't be done (a stationery file that's gone, no account to send from…),
    /// with how often.
    pub not_done: Vec<(String, u64)>,
    /// The other actions done (label, priority, status, junk, subject, forward, redirect, reply,
    /// notify), with how often.
    pub acted: Vec<(String, u64)>,
    /// Transfer / Copy targets that aren't in the library.
    pub missing_folders: Vec<String>,
    /// Messages a "sort" action sent through the spam filter, by where it put them (category,
    /// how many); `moved` counts those it moved.
    pub sorted: Vec<(String, u64)>,
}

const RULES_KEY: &str = "rules";

impl crate::Library {
    /// The rules, in order.
    pub fn rules(&self) -> Result<Vec<Rule>> {
        Ok(match self.setting(RULES_KEY)? {
            Some(json) => serde_json::from_str(&json)?,
            None => Vec::new(),
        })
    }

    pub fn set_rules(&self, rules: &[Rule]) -> Result<()> {
        self.set_setting(RULES_KEY, &serde_json::to_string(rules)?)
    }

    /// A folder by account address (None: the account of `fallback_folder`) and path, any case.
    fn rule_folder(&self, account: Option<&str>, path: &str, fallback_folder: i64) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        let sql = match account {
            Some(_) => "SELECT f.id FROM folders f JOIN accounts a ON a.id = f.account WHERE a.address = ?1 COLLATE NOCASE AND f.path = ?2 COLLATE NOCASE",
            None => "SELECT f.id FROM folders f WHERE f.account = (SELECT account FROM folders WHERE id = ?1) AND f.path = ?2 COLLATE NOCASE",
        };
        let first: Box<dyn rusqlite::ToSql> = match account {
            Some(a) => Box::new(a.to_string()),
            None => Box::new(fallback_folder),
        };
        Ok(self.conn.query_row(sql, rusqlite::params![first, path], |r| r.get(0)).optional()?)
    }

    /// Runs the rules (those for running by hand) on a folder's messages, or only on `only`
    /// (the selection). `dry_run`: says what would happen, changes nothing.
    pub fn run_rules(&self, folder: i64, only: Option<&[i64]>, dry_run: bool, progress: &dyn crate::Progress) -> Result<RunReport> {
        self.run_rules_as(folder, only, dry_run, true, false, progress)
    }

    /// The rules for mail as it arrives, on new messages in every account's Inbox: those stored
    /// since the last time (setting "incoming-rules-after", a message id). The first time only
    /// notes where things stand: rules on arrival don't go back over old mail (Filter Messages
    /// and the spam filter's dry run do that, by choice). Run by the download after storing.
    pub fn run_incoming_rules(&self) -> Result<RunReport> {
        let newest: i64 = self.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0))?;
        // off until the user switches it on (Filters ▸ "Run on arriving mail"): classic filters
        // come in marked "incoming" and must not start filing mail by themselves. While off,
        // the mark keeps up, so switching on never goes back over what came in meanwhile.
        if self.setting(INCOMING_ON_KEY)?.as_deref() != Some("on") {
            self.set_setting(INCOMING_KEY, &newest.to_string())?;
            return Ok(RunReport::default());
        }
        let Some(after) = self.setting(INCOMING_KEY)?.and_then(|v| v.parse::<i64>().ok()) else {
            self.set_setting(INCOMING_KEY, &newest.to_string())?;
            return Ok(RunReport::default());
        };
        if newest <= after {
            return Ok(RunReport::default());
        }
        // arriving mail (each Inbox), and mail the user sent (each Sent folder: the rules
        // marked "outgoing", run once it has gone)
        let mut stmt = self.conn.prepare(
            "SELECT l.folder, l.message, lower(f.path) = 'inbox' FROM locations l JOIN folders f ON f.id = l.folder
             WHERE l.message > ?1 AND l.message <= ?2
               AND (lower(f.path) = 'inbox' OR f.role = 'sent' OR lower(f.path) IN ('sent', 'sent items', 'sent messages', 'sent mail'))
             ORDER BY l.folder, l.message",
        )?;
        let found: Vec<(i64, i64, bool)> = stmt.query_map([after, newest], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut total = RunReport::default();
        let mut by_folder: std::collections::BTreeMap<(i64, bool), Vec<i64>> = Default::default();
        for (folder, message, inbox) in found {
            by_folder.entry((folder, inbox)).or_default().push(message);
        }
        for ((folder, inbox), ids) in by_folder {
            let r = self.run_rules_as(folder, Some(&ids), false, false, inbox, &crate::Silent)?;
            total.add(r);
        }
        self.set_setting(INCOMING_KEY, &newest.to_string())?;
        Ok(total)
    }

    /// `manual`: the rules marked for running by hand; `incoming`: those for arriving mail.
    pub fn run_rules_as(&self, folder: i64, only: Option<&[i64]>, dry_run: bool, manual: bool, incoming: bool, progress: &dyn crate::Progress) -> Result<RunReport> {
        let rules = self.rules()?;
        let mut to_sort: Vec<(i64, i64)> = Vec::new();
        // newest first, chosen messages too
        let ids: Vec<i64> = match only {
            Some(ids) => {
                let mut stmt = self.conn.prepare_cached("SELECT date FROM messages WHERE id = ?1")?;
                let mut dated: Vec<(i64, i64)> = ids.iter().map(|id| (stmt.query_row([id], |r| r.get(0)).unwrap_or(0), *id)).collect();
                dated.sort_by(|a, b| b.cmp(a));
                dated.into_iter().map(|(_, id)| id).collect()
            }
            None => {
                let mut stmt = self.conn.prepare("SELECT message FROM locations WHERE folder = ?1 ORDER BY date DESC")?;
                stmt.query_map([folder], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            }
        };
        let (personality, account): (String, i64) = self.conn.query_row(
            "SELECT COALESCE(a.address, a.name), a.id FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1",
            [folder],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // on arriving mail the spam filter works only in the mailboxes chosen for it (by hand,
        // wherever the user runs the rules)
        // (never the user's own sent mail: an outgoing run is neither manual nor incoming)
        let sort_here = (manual || incoming) && (!incoming || self.spam_filter_accounts()?.contains(&account));
        let mut report = RunReport { by_rule: rules.iter().map(|r| (r.name.clone(), 0)).collect(), ..Default::default() };
        let mut not_done: std::collections::BTreeMap<String, u64> = Default::default();
        let mut acted: std::collections::BTreeMap<String, u64> = Default::default();
        // (each thing done goes into Activity's details: under rules on new mail, or Filter Messages)
        let job: &'static str = if incoming { "rules" } else { "filter" };
        let log = |id: i64, what: String| if !dry_run { let _ = self.log_job(job, Some(id), &what); };
        for (n, id) in ids.iter().enumerate() {
            if progress.canceled() {
                return Err(Error::Canceled);
            }
            progress.report(n as u64, ids.len() as u64, &format!("Running the rules: message {} of {}", n + 1, ids.len()));
            report.checked += 1;
            let facts = Facts::of(&self.raw_message(*id)?, &personality);
            let actions = actions_for(&rules, &facts, manual, incoming);
            if actions.is_empty() {
                continue;
            }
            report.matched += 1;
            let mut counted: Vec<&str> = Vec::new();
            let mut moved = false;
            for (rule, action) in actions {
                if !counted.contains(&rule.name.as_str()) {
                    counted.push(&rule.name);
                    if let Some(entry) = report.by_rule.iter_mut().find(|(name, _)| *name == rule.name) {
                        entry.1 += 1;
                    }
                }
                match action {
                    Action::Transfer { account, folder: path } | Action::Copy { account, folder: path } => {
                        let Some(target) = self.rule_folder(account.as_deref(), path, folder)? else {
                            if !report.missing_folders.contains(path) {
                                report.missing_folders.push(path.clone());
                            }
                            continue;
                        };
                        if matches!(action, Action::Transfer { .. }) {
                            if moved || target == folder {
                                continue;
                            }
                            moved = true;
                            report.moved += 1;
                            if !dry_run {
                                self.move_messages(&[*id], folder, target)?;
                                log(*id, format!("Rule “{}”: moved to {}", rule.name, self.folder_path_of(target)));
                            }
                        } else {
                            report.copied += 1;
                            if !dry_run {
                                self.copy_messages(&[*id], target)?;
                                log(*id, format!("Rule “{}”: copied to {}", rule.name, self.folder_path_of(target)));
                            }
                        }
                    }
                    Action::Delete if !moved => {
                        moved = true;
                        report.deleted += 1;
                        if !dry_run {
                            let account: i64 = self.conn.query_row("SELECT account FROM folders WHERE id = ?1", [folder], |r| r.get(0))?;
                            let trash = self.trash_folder(account)?;
                            self.move_messages(&[*id], folder, trash)?;
                            log(*id, format!("Rule “{}”: deleted (to {})", rule.name, self.folder_path_of(trash)));
                        }
                    }
                    Action::Delete => {}
                    Action::Sort if !moved && sort_here => {
                        moved = true;
                        to_sort.push((*id, folder));
                    }
                    Action::Sort => {}
                    Action::Junk if moved => {}
                    other => {
                        let name = serde_json::to_value(other).ok().and_then(|v| v["do"].as_str().map(str::to_string)).unwrap_or_default();
                        let outcome = if dry_run { Ok(()) } else { self.rule_action(*id, folder, other, &rule.name, &facts) };
                        match outcome {
                            Ok(()) => {
                                log(*id, format!("Rule “{}”: {name}", rule.name));
                                *acted.entry(name).or_default() += 1;
                                if matches!(other, Action::Junk) {
                                    moved = true;
                                    report.moved += 1;
                                }
                            }
                            Err(why) => *not_done.entry(format!("{name}: {why}")).or_default() += 1,
                        }
                    }
                }
            }
        }
        report.not_done = not_done.into_iter().collect();
        report.acted = acted.into_iter().collect();
        if !to_sort.is_empty() {
            progress.report(0, to_sort.len() as u64, &format!("Sorting {} messages with the spam filter", to_sort.len()));
            if dry_run {
                let preview = self.triage_messages(&to_sort, progress)?;
                report.moved += preview.proposals.iter().filter(|p| p.destination.is_some()).count() as u64;
                report.sorted = preview.counts.into_iter().collect();
            } else {
                // new mail: its senders' domains looked up first (how old each is)
                if incoming {
                    let mut stmt = self.conn.prepare_cached(
                        "SELECT a.address FROM message_addresses ma JOIN addresses a ON a.id = ma.address WHERE ma.message = ?1 AND ma.role = 0 ORDER BY ma.position LIMIT 1",
                    )?;
                    let mut domains: Vec<String> = Vec::new();
                    for (id, _) in &to_sort {
                        if let Ok(address) = stmt.query_row([id], |r| r.get::<_, String>(0)) {
                            domains.push(crate::triage_decide::org_domain(address.rsplit('@').next().unwrap_or("")));
                        }
                    }
                    let _ = self.learn_domains(&domains); // (no network: decided without it)
                    let ids: Vec<i64> = to_sort.iter().map(|(id, _)| *id).collect();
                    let _ = self.check_blocklists(&ids); // (no key, or no network: decided without them)
                }
                let (moved, counts) = crate::job_log::as_job(job, || self.triage_now(&to_sort))?;
                report.moved += moved;
                report.sorted = counts;
            }
            report.sorted.sort();
        }
        Ok(report)
    }
}

/// The rule that sorts arriving mail with the spam filter (made by the switch).
pub const SPAM_FILTER_RULE: &str = "Spam filter";

impl crate::Library {
    /// Whether arriving mail is sorted by the spam filter: arriving-mail rules are on and the
    /// spam-filter rule is there and on.
    pub fn spam_filter_on_arrival(&self) -> Result<bool> {
        let on = self.setting(INCOMING_ON_KEY)?.as_deref() == Some("on");
        Ok(on && self.rules()?.iter().any(|r| r.enabled && r.incoming && r.actions.contains(&Action::Sort)))
    }

    /// The one switch (Mail Filter Rules ▸ "Sort arriving mail"): on makes sure the spam-filter
    /// rule is there and on, and turns arriving-mail rules on (only mail arriving from now on:
    /// never old mail); off turns that rule off (other rules for arriving mail keep running).
    pub fn set_spam_filter_on_arrival(&self, on: bool) -> Result<()> {
        let mut rules = self.rules()?;
        match rules.iter_mut().find(|r| r.actions.contains(&Action::Sort) && r.incoming) {
            Some(rule) => rule.enabled = on,
            None if on => rules.insert(0, Rule { name: SPAM_FILTER_RULE.into(), enabled: true, incoming: true, manual: true, all: true, actions: vec![Action::Sort], ..Default::default() }),
            None => {}
        }
        self.set_rules(&rules)?;
        if on {
            self.set_setting(INCOMING_ON_KEY, "on")?;
        }
        Ok(())
    }
}

const INCOMING_KEY: &str = "incoming-rules-after";
/// "on" = the rules marked for arriving mail run on it (off by default).
pub const INCOMING_ON_KEY: &str = "incoming-rules";

impl RunReport {
    /// Adds another run's counts to these (rules on several inboxes).
    pub fn add(&mut self, other: RunReport) {
        self.checked += other.checked;
        self.matched += other.matched;
        self.moved += other.moved;
        self.copied += other.copied;
        self.deleted += other.deleted;
        for (name, n) in other.by_rule {
            match self.by_rule.iter_mut().find(|(k, _)| *k == name) {
                Some(e) => e.1 += n,
                None => self.by_rule.push((name, n)),
            }
        }
        for (list, from) in [(&mut self.not_done, other.not_done), (&mut self.sorted, other.sorted), (&mut self.acted, other.acted)] {
            for (name, n) in from {
                match list.iter_mut().find(|(k, _)| *k == name) {
                    Some(e) => e.1 += n,
                    None => list.push((name, n)),
                }
            }
        }
        for f in other.missing_folders {
            if !self.missing_folders.contains(&f) {
                self.missing_folders.push(f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(raw: &str) -> Facts {
        Facts::of(raw.as_bytes(), "pat@example.com")
    }

    const CLASSIC: &str = "3\r\nrule From:authorize.net\r\ntransfer BUSINESS.FOL\\Card Settlements.mbx\r\nstop \r\nincoming\r\nmanual\r\nheader From:\r\nverb contains\r\nvalue authorize.net\r\nconjunction ignore\r\nheader \r\nverb contains\r\nvalue \r\nrule Lists\r\ntransfer Mailing Lists.fol\\Vendor Updates.mbx\r\nstop \r\nincoming\r\nheader «Any Recipient»\r\nverb contains\r\nvalue patchmanagement\r\nconjunction or\r\nheader Subject:\r\nverb contains\r\nvalue [patch]\r\nrule Not from boss\r\nlabel 3\r\nmanual\r\nheader To:\r\nverb contains\r\nvalue pat\r\nconjunction unless\r\nheader From:\r\nverb is\r\nvalue boss@x.com\r\n";

    #[test]
    fn classic_filters_are_read_whole() {
        let rules = import_classic(CLASSIC, "pat@example.org").unwrap();
        assert_eq!(rules.len(), 3);
        let r = &rules[0];
        assert_eq!(r.name, "From:authorize.net");
        assert!(r.incoming && r.manual && !r.outgoing && r.enabled);
        assert_eq!(r.tests, vec![Test { field: "From:".into(), verb: Verb::Contains, value: "authorize.net".into(), unless: false }]);
        assert_eq!(r.actions, vec![Action::Transfer { account: Some("pat@example.org".into()), folder: "BUSINESS.FOL/Card Settlements".into() }, Action::Stop]);
        assert!(!rules[1].all, "or");
        assert_eq!(rules[1].tests.len(), 2);
        assert!(rules[2].tests[1].unless);
        assert!(import_classic("not a filters file", "x@y").is_err());
    }

    #[test]
    fn rules_match_like_classic() {
        let rules = import_classic(CLASSIC, "a@b").unwrap();
        let invoice = facts("From: AuthorizeNet <noreply@authorize.net>\r\nTo: pat@example.com\r\nSubject: Settlement\r\n\r\nbody");
        assert!(rules[0].matches(&invoice));
        let list = facts("From: x@y.com\r\nTo: other@z.com\r\nSubject: [PATCH] update\r\n\r\nbody");
        assert!(rules[1].matches(&list), "or: the subject matches");
        let boss = facts("From: boss@x.com\r\nTo: pat@example.com\r\nSubject: hi\r\n\r\n");
        let colleague = facts("From: amy@x.com\r\nTo: pat@example.com\r\nSubject: hi\r\n\r\n");
        assert!(!rules[2].matches(&boss), "unless");
        assert!(rules[2].matches(&colleague));

        // the first rule that matches and says Stop ends it
        let got = actions_for(&rules, &invoice, false, true);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn running_rules_on_a_folder() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = crate::Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |name: &str, from: &str, subject: &str| {
            let path = src.join(name);
            std::fs::write(&path, format!("From: {from}\r\nTo: pat@example.com\r\nSubject: {subject}\r\nMessage-ID: <{name}@x>\r\n\r\nbody\r\n")).unwrap();
            path
        };
        let files = vec![mail("1.eml", "noreply@authorize.net", "Settlement"), mail("2.eml", "amy@x.com", "Hello")];
        let options = crate::ImportOptions { destination: Some("Inbox".into()), ..Default::default() };
        crate::import(&mut library, &files, &options, &Silent).unwrap();
        let target = crate::ImportOptions { destination: Some("BUSINESS.FOL/Card Settlements".into()), ..Default::default() };
        crate::import(&mut library, &[mail("3.eml", "a@b.c", "x")], &target, &Silent).unwrap();
        let inbox = library.find_folder("Inbox").unwrap().unwrap();

        let mut rules = import_classic(CLASSIC, "a@b").unwrap();
        for r in &mut rules {
            for a in &mut r.actions {
                if let Action::Transfer { account, .. } = a {
                    *account = None; // (the imported-files account: the folder's own)
                }
            }
        }
        library.set_rules(&rules).unwrap();
        assert_eq!(library.rules().unwrap(), rules);

        let dry = library.run_rules(inbox, None, true, &Silent).unwrap();
        // the transfer rule moves one; the label rule ("To pat, unless from the boss") labels
        // the other
        assert_eq!((dry.checked, dry.matched, dry.moved), (2, 2, 1));
        assert_eq!(dry.acted, vec![("label".to_string(), 1)]);
        assert!(dry.not_done.is_empty());
        assert_eq!(library.count(&[inbox]).unwrap(), 2, "a dry run changes nothing");
        let amy: i64 = library.conn.query_row("SELECT id FROM messages WHERE from_display LIKE '%amy%'", [], |r| r.get(0)).unwrap();
        assert!(library.labels_on(amy).unwrap().is_empty(), "nor labels anything");
        let done = library.run_rules(inbox, None, false, &Silent).unwrap();
        assert_eq!(done.moved, 1);
        assert_eq!(done.by_rule[0], ("From:authorize.net".to_string(), 1));
        assert_eq!(library.count(&[inbox]).unwrap(), 1);
        assert_eq!(library.labels_on(amy).unwrap().len(), 1, "labeled (classic's label number, as a name)");
    }

    #[test]
    fn outgoing_rules_act_on_mail_once_it_is_sent() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = crate::Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let mail = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("From: pat@example.com\r\nTo: client@acme.com\r\nSubject: Quote {name}\r\nMessage-ID: <{name}@x>\r\n\r\nbody\r\n")).unwrap();
            path
        };
        let rule = Rule {
            name: "Clients".into(),
            enabled: true,
            outgoing: true,
            tests: vec![Test { field: "To:".into(), verb: Verb::Contains, value: "acme.com".into(), unless: false }],
            all: true,
            actions: vec![Action::Label { label: "Prospects".into() }, Action::Sort],
            ..Default::default()
        };
        library.set_rules(&[rule]).unwrap();
        library.set_setting(INCOMING_ON_KEY, "on").unwrap();
        library.run_incoming_rules().unwrap(); // (the first time notes where things stand)
        let sent = crate::ImportOptions { destination: Some("Sent Items".into()), ..Default::default() };
        crate::import(&mut library, &[mail("a.eml")], &sent, &Silent).unwrap();
        let r = library.run_incoming_rules().unwrap();
        assert_eq!((r.checked, r.matched, r.moved), (1, 1, 0), "labeled, never sorted as spam: {r:?}");
        let id: i64 = library.conn.query_row("SELECT id FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(library.labels_on(id).unwrap(), vec!["Prospects".to_string()]);
    }

    #[test]
    fn the_spam_filter_as_a_rule_on_arriving_mail_and_by_hand() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = crate::Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |name: &str, extra: &str| {
            let path = src.join(name);
            std::fs::write(&path, format!("From: Deals <news@shop.example>\r\nTo: pat@example.com\r\nSubject: Sale {name}\r\nMessage-ID: <{name}@x>\r\n{extra}\r\nbody\r\n")).unwrap();
            path
        };
        let inbox_mail = crate::ImportOptions { destination: Some("Inbox".into()), ..Default::default() };
        crate::import(&mut library, &[mail("old.eml", "List-Unsubscribe: <https://shop.example/u>\r\n")], &inbox_mail, &Silent).unwrap();
        let inbox = library.find_folder("Inbox").unwrap().unwrap();
        // the spam filter works only in the mailboxes chosen for it (imported files: by choice)
        library.conn.execute("UPDATE accounts SET address = 'pat@example.com' WHERE id = (SELECT account FROM folders WHERE id = ?1)", [inbox]).unwrap();
        library.set_spam_filter_mailboxes(&["pat@example.com".into()]).unwrap();
        // "sort everything that arrives": no tests needed for a spam-filter rule
        let rule = Rule { name: "Spam filter".into(), enabled: true, incoming: true, manual: true, all: true, actions: vec![Action::Sort], ..Default::default() };
        library.set_rules(&[rule]).unwrap();

        // off until switched on: mail arriving meanwhile is never gone back over
        crate::import(&mut library, &[mail("while-off.eml", "List-Unsubscribe: <https://shop.example/u>\r\n")], &inbox_mail, &Silent).unwrap();
        assert_eq!(library.run_incoming_rules().unwrap().checked, 0);
        assert_eq!(library.count(&[inbox]).unwrap(), 2);
        library.set_setting(INCOMING_ON_KEY, "on").unwrap();
        assert_eq!(library.run_incoming_rules().unwrap().checked, 0, "what came in while off stays");
        assert_eq!(library.count(&[inbox]).unwrap(), 2);
        crate::import(&mut library, &[mail("new.eml", "List-Unsubscribe: <https://shop.example/u>\r\n")], &inbox_mail, &Silent).unwrap();
        let r = library.run_incoming_rules().unwrap();
        assert_eq!((r.checked, r.moved), (1, 1), "{r:?}");
        assert_eq!(r.sorted, vec![("bulk".to_string(), 1)]);
        assert_eq!(library.count(&[inbox]).unwrap(), 2, "the new one went to Bulk, the two older ones stay");
        assert_eq!(library.run_incoming_rules().unwrap().checked, 0, "nothing new");

        // by hand on the old one (a selection): dry run first, then for real
        let old = library.list_ids(&[inbox], &[]).unwrap()[0].0;
        let dry = library.run_rules(inbox, Some(&[old]), true, &Silent).unwrap();
        assert_eq!((dry.moved, library.count(&[inbox]).unwrap()), (1, 2));
        library.run_rules(inbox, Some(&[old]), false, &Silent).unwrap();
        assert_eq!(library.count(&[inbox]).unwrap(), 1);
        let bulk = library.find_folder(crate::triage_decide::MARKETING_FOLDER).unwrap().unwrap();
        assert_eq!(library.count(&[bulk]).unwrap(), 2);

        // a mailbox not chosen for the spam filter: arriving mail there stays put
        library.set_spam_filter_mailboxes(&[]).unwrap();
        crate::import(&mut library, &[mail("later.eml", "List-Unsubscribe: <https://shop.example/u>\r\n")], &inbox_mail, &Silent).unwrap();
        let r = library.run_incoming_rules().unwrap();
        assert_eq!((r.checked, r.moved), (1, 0), "{r:?}");
        assert_eq!(library.count(&[inbox]).unwrap(), 2);
    }

    #[test]
    fn one_switch_sorts_arriving_mail_with_the_spam_filter() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let library = crate::Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        assert!(!library.spam_filter_on_arrival().unwrap(), "off until switched on");
        library.set_spam_filter_on_arrival(true).unwrap();
        assert!(library.spam_filter_on_arrival().unwrap());
        assert_eq!(library.rules().unwrap().iter().filter(|r| r.actions.contains(&Action::Sort)).count(), 1);
        library.set_spam_filter_on_arrival(false).unwrap();
        assert!(!library.spam_filter_on_arrival().unwrap());
        library.set_spam_filter_on_arrival(true).unwrap();
        assert_eq!(library.rules().unwrap().len(), 1, "switched on again: the same rule, not a second");
    }

    #[test]
    fn verbs_and_classic_regex() {
        let f = facts("From: Ann <ann@example.com>\r\nSubject: Invoice 12345 due\r\nX-Spam-Score: 7.5\r\n\r\nPlease pay");
        let t = |field: &str, verb: Verb, value: &str| Test { field: field.into(), verb, value: value.into(), unless: false }.matches(&f);
        assert!(t("Subject:", Verb::StartsWith, "invoice"));
        assert!(t("Subject:", Verb::EndsWith, "DUE"));
        assert!(t("Subject:", Verb::Regex, "Invoice [0-9]+ due$"));
        assert!(!t("Subject:", Verb::Regex, "^due"));
        assert!(t("Subject:", Verb::RegexIgnoreCase, "INVOICE \\d+"));
        assert!(t("X-Spam-Score:", Verb::Greater, "5"));
        assert!(t("X-Spam-Score:", Verb::Appears, ""));
        assert!(t("Reply-To:", Verb::NotAppears, ""));
        assert!(t("«Body»", Verb::Contains, "pay"));
        assert!(t("«Any Header»", Verb::Contains, "ann@example"));
        assert!(t("From:", Verb::NotContains, "bob"));
    }
}
