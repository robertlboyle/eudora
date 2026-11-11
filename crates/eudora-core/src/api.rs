//! The local API: what other programs and plugins may do with the mail, and only that
//! (docs/API.md).
//!
//! - **Nothing is reachable until the user grants it.** A program asks (`v1.auth.request`: its
//!   name, publisher, why, the permissions); the window shows the user what it asks for, with
//!   a tick per permission; once granted the program collects its own token (`v1.auth.collect`,
//!   with the secret only it knows), and Eudora keeps only the token's hash.
//! - **Narrow, named permissions** (`Permission`), each limited to some accounts and folders if
//!   the user likes. Secrets (passwords, sign-ins, keys) have no method at all.
//! - **Sending is confirmed by the user, message by message**, until the user allows all from
//!   that program ("send without asking"); revocable.
//! - **Rate limits** per client, so a "read everything" program is slowed and seen.
//! - **Everything it does is logged, in the library** (`api_log`): every read and write with
//!   what it touched (ids, folders, counts, never the mail itself); full by default, turned down
//!   per client once trusted ("changes": writes only; "refusals"); refusals, grants and
//!   revocations are always logged.
//!
//! One table of methods (`METHODS`) holds each method's permission and whether it changes
//! anything; the transport (`api_server`) and the reference are made from it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::library::{Library, now};

/// What a program or plugin may be allowed to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Permission {
    /// Accounts' addresses and names (never their secrets).
    #[serde(rename = "accounts.read")]
    AccountsRead,
    /// Who sent it, the subject, date, folders, labels, status: not the text.
    #[serde(rename = "mail.headers.read")]
    HeadersRead,
    /// The full text of messages.
    #[serde(rename = "mail.bodies.read")]
    BodiesRead,
    #[serde(rename = "mail.attachments.read")]
    AttachmentsRead,
    /// Searching (results only as far as the other grants let it read).
    #[serde(rename = "mail.search")]
    Search,
    /// Filing, labels, status, priority.
    #[serde(rename = "mail.change")]
    Change,
    #[serde(rename = "mail.delete")]
    Delete,
    /// Sending (each message confirmed by the user unless allowed without asking).
    #[serde(rename = "mail.send")]
    Send,
    #[serde(rename = "rules.read")]
    RulesRead,
    #[serde(rename = "rules.write")]
    RulesWrite,
    /// Writing mail: drafts, replies and forwards started from a message, spelling, addresses.
    #[serde(rename = "mail.compose")]
    Compose,
    /// The spam filter: sorting, Spam / Not Spam, its rules, its lists, learning.
    #[serde(rename = "spam.filter")]
    SpamFilter,
    /// Settings: labels, signatures, views, pictures, completion, the window's choices.
    #[serde(rename = "settings.read")]
    SettingsRead,
    #[serde(rename = "settings.write")]
    SettingsWrite,
    /// Adding and changing accounts (servers, shared mailboxes, names), signing in and out.
    #[serde(rename = "accounts.manage")]
    AccountsManage,
    /// What Eudora is doing: downloads, changes waiting, sync, statistics.
    #[serde(rename = "status.read")]
    StatusRead,
    /// Running the library: check mail, pause, offline, back up, restore, import, optimize,
    /// sync, plugins, the firewall.
    #[serde(rename = "library.manage")]
    LibraryManage,
    /// The address book: names, addresses, numbers, notes.
    #[serde(rename = "contacts.read")]
    ContactsRead,
    /// Adding, changing, merging and deleting contacts (on the servers too).
    #[serde(rename = "contacts.write")]
    ContactsWrite,
    /// Calendars and their events: times, subjects, places, people.
    #[serde(rename = "calendar.read")]
    CalendarRead,
    /// Adding, changing and deleting events and answering invitations (on the servers too).
    #[serde(rename = "calendar.write")]
    CalendarWrite,
    /// To Do tasks: what they say, when due, the messages they're about.
    #[serde(rename = "todo.read")]
    TodoRead,
    /// Adding, changing, finishing and deleting tasks, flagging messages, sending tasks to Twos.
    #[serde(rename = "todo.write")]
    TodoWrite,
}

impl Permission {
    pub const ALL: [Permission; 23] = [
        Permission::AccountsRead,
        Permission::HeadersRead,
        Permission::BodiesRead,
        Permission::AttachmentsRead,
        Permission::Search,
        Permission::Change,
        Permission::Delete,
        Permission::Send,
        Permission::RulesRead,
        Permission::RulesWrite,
        Permission::Compose,
        Permission::SpamFilter,
        Permission::SettingsRead,
        Permission::SettingsWrite,
        Permission::AccountsManage,
        Permission::StatusRead,
        Permission::LibraryManage,
        Permission::ContactsRead,
        Permission::ContactsWrite,
        Permission::CalendarRead,
        Permission::CalendarWrite,
        Permission::TodoRead,
        Permission::TodoWrite,
    ];

    /// What the user is told in the grant window.
    pub fn plainly(self) -> &'static str {
        match self {
            Permission::AccountsRead => "see your accounts' addresses and names",
            Permission::HeadersRead => "see who your mail is from, its subjects and dates (not the text)",
            Permission::BodiesRead => "read the full text of your mail",
            Permission::AttachmentsRead => "open your attachments",
            Permission::Search => "search your mail",
            Permission::Change => "file mail, mark it read or unread, change labels and priority",
            Permission::Delete => "delete mail",
            Permission::Send => "send mail as you (each message shown to you first)",
            Permission::RulesRead => "see your rules",
            Permission::RulesWrite => "change your rules",
            Permission::Compose => "write mail: drafts, replies and forwards (with the message they answer), spelling, address suggestions",
            Permission::SpamFilter => "use and change the spam filter (it moves mail)",
            Permission::SettingsRead => "see your preferences (labels, signatures, views, the window's choices)",
            Permission::SettingsWrite => "change your preferences",
            Permission::AccountsManage => "add and change accounts and their servers, and sign in or out (it can make Eudora connect to servers it names)",
            Permission::StatusRead => "see what Eudora is doing (downloads, changes waiting, sync, statistics)",
            Permission::LibraryManage => "run your library: check mail, pause, work offline, back up (copying all your mail to a file), restore, import, optimize, sync, plugins",
            Permission::ContactsRead => "see your address book (names, addresses, phone numbers, notes)",
            Permission::ContactsWrite => "add, change, merge and delete your contacts, on the servers too",
            Permission::CalendarRead => "see your calendars (meetings, times, places, who's invited)",
            Permission::CalendarWrite => "add, change and delete events and answer invitations, on the servers too",
            Permission::TodoRead => "see your To Do tasks",
            Permission::TodoWrite => "add, change and delete your To Do tasks and flag messages (in Microsoft To Do too)",
        }
    }

    /// The strongest ones get a warning of their own in the grant window.
    pub fn sensitive(self) -> bool {
        matches!(
            self,
            Permission::BodiesRead
                | Permission::AttachmentsRead
                | Permission::Send
                | Permission::Delete
                | Permission::RulesWrite
                | Permission::SpamFilter
                | Permission::SettingsWrite
                | Permission::AccountsManage
                | Permission::LibraryManage
                | Permission::ContactsRead
                | Permission::ContactsWrite
                | Permission::CalendarRead
                | Permission::CalendarWrite
        )
    }

    /// Calls allowed per minute (a client's own budget for this kind of call).
    fn per_minute(self) -> u32 {
        match self {
            Permission::BodiesRead | Permission::AttachmentsRead => 1_000,
            Permission::Send => 20,
            Permission::Delete | Permission::Change | Permission::RulesWrite | Permission::SpamFilter | Permission::SettingsWrite => 600,
            Permission::AccountsManage | Permission::LibraryManage => 120,
            Permission::ContactsWrite | Permission::CalendarWrite | Permission::TodoWrite => 600,
            _ => 6_000,
        }
    }
}

/// A permission, limited to some accounts (addresses) and folders (paths) when not empty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub permission: Permission,
    #[serde(default)]
    pub accounts: Vec<String>,
    #[serde(default)]
    pub folders: Vec<String>,
}

/// How much of a client's activity is logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Every read and write (the default).
    Full,
    /// Writes only.
    Changes,
    /// Only what was refused.
    Refusals,
}

impl LogLevel {
    fn parse(s: &str) -> LogLevel {
        match s {
            "changes" => LogLevel::Changes,
            "refusals" => LogLevel::Refusals,
            _ => LogLevel::Full,
        }
    }

    fn name(self) -> &'static str {
        match self {
            LogLevel::Full => "full",
            LogLevel::Changes => "changes",
            LogLevel::Refusals => "refusals",
        }
    }
}

/// A program or plugin as Settings ▸ Programs and plugins lists it.
#[derive(Clone, Debug, Serialize)]
pub struct Client {
    pub id: i64,
    pub name: String,
    pub publisher: String,
    pub kind: String,
    pub grants: Vec<Grant>,
    pub log_level: LogLevel,
    pub send_without_asking: bool,
    pub created: i64,
    pub last_used: Option<i64>,
    pub revoked: Option<i64>,
    /// Granted (has a token); false: still asking.
    pub granted: bool,
}

/// A program's request waiting for the user.
#[derive(Clone, Debug, Serialize)]
pub struct Request {
    pub id: i64,
    pub client: i64,
    pub name: String,
    pub publisher: String,
    pub kind: String,
    pub reason: String,
    pub asked: Vec<Grant>,
    pub created: i64,
}

/// A message an API client wants sent, waiting for the user's OK.
#[derive(Clone, Debug, Serialize)]
pub struct PendingSend {
    pub id: i64,
    pub client: i64,
    pub client_name: String,
    pub draft: crate::compose::Draft,
    pub created: i64,
}

/// One line of the API log.
#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub at: i64,
    pub client: Option<i64>,
    pub client_name: Option<String>,
    pub method: String,
    pub write: bool,
    pub allowed: bool,
    pub detail: String,
}

/// Where the API may be served (this computer's own setting, never synced).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiSettings {
    /// While Eudora's window is open.
    pub with_window: bool,
    /// Also without the window (`eudora-cli api serve`): off unless the user overrides (2026-10-16).
    pub without_window: bool,
}

impl Default for ApiSettings {
    fn default() -> Self {
        ApiSettings { with_window: true, without_window: false }
    }
}

pub const SETTINGS_KEY: &str = "api-settings";

impl Library {
    pub fn api_settings(&self) -> Result<ApiSettings> {
        Ok(self.setting(SETTINGS_KEY)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    pub fn set_api_settings(&self, s: &ApiSettings) -> Result<()> {
        self.set_setting(SETTINGS_KEY, &serde_json::to_string(s)?)?;
        self.api_log_line(
            None,
            "settings",
            true,
            true,
            &format!("API {} while Eudora is open, {} without it", if s.with_window { "on" } else { "off" }, if s.without_window { "allowed" } else { "off" }),
        )
    }
}

/// One API method: its name, what it needs, whether it changes anything, what it does.
pub struct Method {
    pub name: &'static str,
    /// None: anyone may call it (asking for access, collecting a token).
    pub permission: Option<Permission>,
    pub write: bool,
    pub params: &'static str,
    pub summary: &'static str,
}

/// Every method of version 1. The reference (docs and `v1.api.methods`) is made from this.
/// Every method: these, and the rest of what the window does (`api_more::MORE`).
pub fn all_methods() -> impl Iterator<Item = &'static Method> {
    METHODS.iter().chain(crate::api_more::MORE.iter())
}

fn find_method(name: &str) -> Option<&'static Method> {
    all_methods().find(|m| m.name == name)
}

/// What only the program serving the API can do (the window, or `eudora-cli api serve`).
pub trait Host: Sync {
    /// Runs `work`, which opens the library for writing itself (a backup, an import,
    /// Optimize…): the window lets go of its download meanwhile.
    fn exclusive(&self, work: &mut dyn FnMut() -> Result<Value>) -> Result<Value> {
        work()
    }
    /// The window's own background download: its state, and new options (`set`).
    fn background(&self, _set: Option<&Value>) -> Result<Value> {
        Err(Error::Other("only Eudora's window runs a background download".into()))
    }
}

/// A host with nothing of its own (the command line, tests).
pub struct NoHost;
impl Host for NoHost {}

pub const METHODS: &[Method] = &[
    Method { name: "v1.auth.request", permission: None, write: false, params: "{name, publisher, reason, permissions: [{permission, accounts?, folders?}], secret}", summary: "Asks the user for access; returns the request's id" },
    Method { name: "v1.auth.collect", permission: None, write: false, params: "{request, secret}", summary: "Once the user answered: the token (handed over once), or refused / waiting" },
    Method { name: "v1.api.methods", permission: None, write: false, params: "{}", summary: "Every method, its permission and parameters" },
    Method { name: "v1.api.me", permission: None, write: false, params: "{}", summary: "What this client may do" },
    Method { name: "v1.accounts.list", permission: Some(Permission::AccountsRead), write: false, params: "{}", summary: "Accounts: address, name, kind, message count" },
    Method { name: "v1.folders.list", permission: Some(Permission::HeadersRead), write: false, params: "{}", summary: "Folders it may see, with their message counts" },
    Method { name: "v1.mail.list", permission: Some(Permission::HeadersRead), write: false, params: "{folder, after?: {date, id}, limit? (≤ 500)}", summary: "A folder's messages, newest first, a page at a time" },
    Method { name: "v1.mail.search", permission: Some(Permission::Search), write: false, params: "{query, limit? (≤ 500), everywhere?}", summary: "Search (everything the window understands); only messages it may see; mailboxes left out of search are left out unless everywhere" },
    Method { name: "v1.mail.headers", permission: Some(Permission::HeadersRead), write: false, params: "{id}", summary: "A message's sender, recipients, subject, date, folders, labels, status, attachments' names" },
    Method { name: "v1.mail.body", permission: Some(Permission::BodiesRead), write: false, params: "{id}", summary: "A message's text and HTML" },
    Method { name: "v1.mail.attachment", permission: Some(Permission::AttachmentsRead), write: false, params: "{id, part}", summary: "An attachment's bytes (base64)" },
    Method { name: "v1.mail.move", permission: Some(Permission::Change), write: true, params: "{ids, from, to}", summary: "Files messages into another folder (the server too)" },
    Method { name: "v1.mail.mark", permission: Some(Permission::Change), write: true, params: "{ids, status?, priority?, label?, unlabel?}", summary: "Status, priority, labels" },
    Method { name: "v1.mail.delete", permission: Some(Permission::Delete), write: true, params: "{ids, from}", summary: "Moves messages to the account's Trash" },
    Method { name: "v1.mail.send", permission: Some(Permission::Send), write: true, params: "{draft: {from, to, cc?, bcc?, subject, html?, text?}, send_at?: unix seconds (Send Later)}", summary: "Sends a message: the user confirms it first unless they allowed this client to send without asking" },
    Method { name: "v1.mail.send_status", permission: Some(Permission::Send), write: false, params: "{send}", summary: "Whether a message waiting for the user was sent or refused" },
    Method { name: "v1.rules.list", permission: Some(Permission::RulesRead), write: false, params: "{}", summary: "The rules (filters)" },
    Method { name: "v1.rules.set", permission: Some(Permission::RulesWrite), write: true, params: "{rules}", summary: "Replaces the rules" },
];

/// Per-client call budgets (one minute windows), shared by every connection.
#[derive(Default)]
pub struct Limits {
    windows: Mutex<HashMap<(i64, Permission), (Instant, u32)>>,
}

impl Limits {
    fn allow(&self, client: i64, p: Permission, cost: u32) -> bool {
        let mut windows = self.windows.lock().unwrap();
        let entry = windows.entry((client, p)).or_insert((Instant::now(), 0));
        if entry.0.elapsed().as_secs() >= 60 {
            *entry = (Instant::now(), 0);
        }
        if entry.1 + cost > p.per_minute() {
            return false;
        }
        entry.1 += cost;
        true
    }
}

/// A refusal the caller sees (JSON-RPC error codes: -32001 not granted, -32002 rate limit,
/// -32601 no such method, -32602 bad parameters).
#[derive(Debug)]
pub struct Refusal {
    pub code: i64,
    pub message: String,
}

impl Refusal {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Refusal {
        Refusal { code, message: message.into() }
    }
}

impl From<Error> for Refusal {
    fn from(e: Error) -> Refusal {
        Refusal::new(-32000, e.to_string())
    }
}

impl From<rusqlite::Error> for Refusal {
    fn from(e: rusqlite::Error) -> Refusal {
        Refusal::from(Error::from(e))
    }
}

type Answer = std::result::Result<Value, Refusal>;

/// A new random token or secret (32 bytes, URL-safe base64), with a prefix saying what it is.
fn random_token(prefix: &str) -> Result<String> {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    Ok(format!("{prefix}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)))
}

fn hash(secret: &str) -> Vec<u8> {
    blake3::hash(secret.as_bytes()).as_bytes().to_vec()
}

impl Library {
    // ── the user's side (the window, Settings ▸ Programs and plugins) ──

    pub fn api_requests(&self) -> Result<Vec<Request>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.client, c.name, c.publisher, c.kind, r.reason, r.asked, r.created FROM api_requests r JOIN api_clients c ON c.id = r.client
             WHERE r.answer IS NULL ORDER BY r.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Request {
                id: r.get(0)?,
                client: r.get(1)?,
                name: r.get(2)?,
                publisher: r.get(3)?,
                kind: r.get(4)?,
                reason: r.get(5)?,
                asked: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                created: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The user's answer: `grants` (what they ticked; may be less than asked) or None = refused.
    pub fn api_answer(&self, request: i64, grants: Option<&[Grant]>) -> Result<()> {
        let client: i64 = self.conn.query_row("SELECT client FROM api_requests WHERE id = ?1 AND answer IS NULL", params![request], |r| r.get(0))?;
        self.in_transaction(|| {
            match grants {
                Some(grants) => {
                    let token = random_token("eud1_")?;
                    self.conn.execute(
                        "UPDATE api_clients SET token_hash = ?2, grants = ?3, revoked = NULL WHERE id = ?1",
                        params![client, hash(&token), serde_json::to_string(grants)?],
                    )?;
                    self.conn.execute("UPDATE api_requests SET answer = 'granted', token = ?2 WHERE id = ?1", params![request, token])?;
                    let what: Vec<String> = grants.iter().map(describe).collect();
                    self.api_log_line(Some(client), "grant", true, true, &format!("granted: {}", what.join("; ")))?;
                }
                None => {
                    self.conn.execute("UPDATE api_requests SET answer = 'refused' WHERE id = ?1", params![request])?;
                    self.api_log_line(Some(client), "grant", true, false, "refused by the user")?;
                }
            }
            Ok(())
        })
    }

    pub fn api_clients(&self) -> Result<Vec<Client>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, publisher, kind, grants, log_level, send_without_asking, created, last_used, revoked, token_hash IS NOT NULL FROM api_clients ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Client {
                id: r.get(0)?,
                name: r.get(1)?,
                publisher: r.get(2)?,
                kind: r.get(3)?,
                grants: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                log_level: LogLevel::parse(&r.get::<_, String>(5)?),
                send_without_asking: r.get(6)?,
                created: r.get(7)?,
                last_used: r.get(8)?,
                revoked: r.get(9)?,
                granted: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Revokes a client: its token stops working at once.
    pub fn api_revoke(&self, client: i64) -> Result<()> {
        self.conn.execute("UPDATE api_clients SET revoked = ?2, token_hash = NULL WHERE id = ?1", params![client, now()])?;
        self.api_log_line(Some(client), "revoke", true, true, "revoked by the user")
    }

    /// How much of a client's activity is logged, and whether it may send without asking.
    pub fn api_set_trust(&self, client: i64, log_level: LogLevel, send_without_asking: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE api_clients SET log_level = ?2, send_without_asking = ?3 WHERE id = ?1",
            params![client, log_level.name(), send_without_asking],
        )?;
        self.api_log_line(
            Some(client),
            "trust",
            true,
            true,
            &format!("log: {}; sends {}", log_level.name(), if send_without_asking { "without asking" } else { "confirmed one by one" }),
        )
    }

    pub fn api_pending_sends(&self) -> Result<Vec<PendingSend>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.client, c.name, s.draft, s.created FROM api_sends s JOIN api_clients c ON c.id = s.client WHERE s.answer IS NULL ORDER BY s.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, client, client_name, draft, created) = row?;
            out.push(PendingSend { id, client, client_name, draft: serde_json::from_str(&draft)?, created });
        }
        Ok(out)
    }

    /// The user's answer to a message a client wants sent: OK puts it in the Out mailbox to go
    /// now; `trust`: and let this client send without asking from now on.
    pub fn api_answer_send(&self, send: i64, ok: bool, trust: bool) -> Result<()> {
        let (client, draft): (i64, String) = self.conn.query_row("SELECT client, draft FROM api_sends WHERE id = ?1 AND answer IS NULL", params![send], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if ok {
            let stored: Value = serde_json::from_str(&draft)?;
            let send_at = stored["send_at"].as_i64().filter(|t| *t > now()).unwrap_or_else(now);
            let draft: crate::compose::Draft = serde_json::from_value(stored)?;
            self.queue_message(&draft, send_at)?;
            if trust {
                self.conn.execute("UPDATE api_clients SET send_without_asking = 1 WHERE id = ?1", params![client])?;
                self.api_log_line(Some(client), "trust", true, true, "sends allowed without asking")?;
            }
        }
        self.conn.execute("UPDATE api_sends SET answer = ?2, answered = ?3 WHERE id = ?1", params![send, if ok { "sent" } else { "refused" }, now()])?;
        self.api_log_line(Some(client), "v1.mail.send", true, ok, &format!("send {send} {} by the user", if ok { "confirmed" } else { "refused" }))
    }

    /// The API log, newest first (one client, or every one).
    pub fn api_log(&self, client: Option<i64>, limit: usize) -> Result<Vec<LogLine>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.at, l.client, c.name, l.method, l.write, l.allowed, l.detail FROM api_log l LEFT JOIN api_clients c ON c.id = l.client
             WHERE ?1 IS NULL OR l.client = ?1 ORDER BY l.id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![client, limit as i64], |r| {
            Ok(LogLine { at: r.get(0)?, client: r.get(1)?, client_name: r.get(2)?, method: r.get(3)?, write: r.get(4)?, allowed: r.get(5)?, detail: r.get(6)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Log lines older than 90 days go.
    pub fn api_log_prune(&self) -> Result<u64> {
        Ok(self.conn.execute("DELETE FROM api_log WHERE at < ?1", params![now() - 90 * 86_400])? as u64)
    }

    fn api_log_line(&self, client: Option<i64>, method: &str, write: bool, allowed: bool, detail: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO api_log (at, client, method, write, allowed, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now(), client, method, write, allowed, detail],
        )?;
        Ok(())
    }

    // ── the program's side: one call ──

    /// Answers one call. `token`: what the client presented (None for asking / collecting).
    pub fn api_call(&self, limits: &Limits, host: &dyn Host, token: Option<&str>, method: &str, params: &Value) -> Answer {
        let Some(m) = find_method(method) else {
            return Err(Refusal::new(-32601, format!("there's no method {method} (v1.api.methods lists them)")));
        };
        let Some(permission) = m.permission else {
            return self.open_method(token, method, params);
        };
        let client = self.client_by_token(token)?;
        self.call_as_client(limits, host, client, m, permission, method, params)
    }

    /// A call by the owner of the computer (the command line: `eudora-cli call`): every method,
    /// no permission needed, logged as "command line".
    pub fn api_owner_call(&self, host: &dyn Host, method: &str, params: &Value) -> Answer {
        let Some(m) = find_method(method) else {
            return Err(Refusal::new(-32601, format!("there's no method {method} (eudora-cli api methods lists them)")));
        };
        if m.permission.is_none() {
            return self.open_method(None, method, params);
        }
        let owner = Client {
            id: 0,
            name: "command line".into(),
            publisher: "you".into(),
            kind: "owner".into(),
            grants: Permission::ALL.iter().map(|p| Grant { permission: *p, accounts: vec![], folders: vec![] }).collect(),
            log_level: LogLevel::Full,
            send_without_asking: true,
            created: 0,
            last_used: None,
            revoked: None,
            granted: true,
        };
        let scope = Scope { accounts: None, folders: None, addresses: None, folder_account: Scope::folder_accounts(self)? };
        let result = self.run_method(host, &owner, &scope, method, params);
        let (allowed, detail) = match &result {
            Ok((_, d)) => (true, d.clone()),
            Err(r) => (false, r.message.clone()),
        };
        self.api_log_line(None, method, m.write, allowed, &format!("command line: {detail}"))?;
        result.map(|(v, _)| v)
    }

    /// A call from a plugin (Eudora runs it, so it has no token: it's known by its client id).
    /// The same checks, limits and log as any program's.
    pub fn api_call_as(&self, limits: &Limits, host: &dyn Host, client: i64, method: &str, params: &Value) -> Answer {
        let Some(m) = find_method(method) else {
            return Err(Refusal::new(-32601, format!("there's no method {method}")));
        };
        let Some(permission) = m.permission else {
            return match method {
                "v1.api.methods" => self.open_method(None, method, params),
                _ => Err(Refusal::new(-32601, format!("{method} isn't for plugins"))),
            };
        };
        let found = self.api_clients()?.into_iter().find(|c| c.id == client && c.revoked.is_none() && c.granted);
        let Some(client) = found else {
            self.api_log_line(Some(client), method, m.write, false, "not allowed anything (yet, or any more)")?;
            return Err(Refusal::new(-32001, "this plugin isn't allowed anything (yet, or any more)"));
        };
        self.call_as_client(limits, host, client, m, permission, method, params)
    }

    #[allow(clippy::too_many_arguments)]
    fn call_as_client(&self, limits: &Limits, host: &dyn Host, client: Client, m: &Method, permission: Permission, method: &str, params: &Value) -> Answer {
        if !client.grants.iter().any(|g| g.permission == permission) {
            self.api_log_line(Some(client.id), method, m.write, false, &format!("not granted: {}", serde_json::to_value(permission).unwrap_or_default()))?;
            return Err(Refusal::new(-32001, format!("this program wasn't allowed to {} (ask with v1.auth.request)", permission.plainly())));
        }
        let cost = match params["ids"].as_array() {
            Some(ids) => ids.len().max(1) as u32,
            None => 1,
        };
        if !limits.allow(client.id, permission, cost) {
            self.api_log_line(Some(client.id), method, m.write, false, "over its rate limit")?;
            return Err(Refusal::new(-32002, format!("too many calls: at most {} a minute for this", permission.per_minute())));
        }
        self.conn.execute("UPDATE api_clients SET last_used = ?2 WHERE id = ?1", params![client.id, now()])?;
        let scope = Scope::of(self, &client, permission)?;
        let result = self.run_method(host, &client, &scope, method, params);
        let (allowed, detail) = match &result {
            Ok((_, detail)) => (true, detail.clone()),
            Err(r) => (false, r.message.clone()),
        };
        let wanted = match client.log_level {
            LogLevel::Full => true,
            LogLevel::Changes => m.write || !allowed,
            LogLevel::Refusals => !allowed,
        };
        if wanted {
            self.api_log_line(Some(client.id), method, m.write, allowed, &detail)?;
        }
        result.map(|(v, _)| v)
    }

    fn client_by_token(&self, token: Option<&str>) -> std::result::Result<Client, Refusal> {
        let token = token.ok_or_else(|| Refusal::new(-32001, "no token: ask the user first (v1.auth.request)"))?;
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM api_clients WHERE token_hash = ?1 AND revoked IS NULL AND (expires IS NULL OR expires > ?2)", params![hash(token), now()], |r| r.get(0))
            .optional()
            .map_err(Error::from)?;
        let id = id.ok_or_else(|| Refusal::new(-32001, "this token isn't valid (revoked, expired or never granted)"))?;
        Ok(self.api_clients()?.into_iter().find(|c| c.id == id).ok_or_else(|| Refusal::new(-32001, "no such client"))?)
    }

    fn open_method(&self, token: Option<&str>, method: &str, params: &Value) -> Answer {
        match method {
            "v1.api.methods" => Ok(Value::Array(
                all_methods()
                    .map(|m| json!({ "name": m.name, "permission": m.permission, "write": m.write, "params": m.params, "summary": m.summary }))
                    .collect(),
            )),
            "v1.api.me" => {
                let c = self.client_by_token(token)?;
                Ok(json!({ "name": c.name, "grants": c.grants, "send_without_asking": c.send_without_asking, "log": c.log_level }))
            }
            "v1.auth.request" => {
                let text = |k: &str, max: usize| -> std::result::Result<String, Refusal> {
                    let v = params[k].as_str().map(str::trim).unwrap_or_default();
                    if v.is_empty() || v.len() > max {
                        return Err(Refusal::new(-32602, format!("{k}: 1 to {max} characters")));
                    }
                    Ok(v.to_string())
                };
                let (name, publisher, reason, secret) = (text("name", 80)?, text("publisher", 80)?, text("reason", 500)?, text("secret", 200)?);
                let asked: Vec<Grant> = serde_json::from_value(params["permissions"].clone()).map_err(|e| Refusal::new(-32602, format!("permissions: {e}")))?;
                if asked.is_empty() {
                    return Err(Refusal::new(-32602, "permissions: ask for at least one"));
                }
                // a flood of requests is refused (the user answers them one by one)
                let waiting: i64 = self.conn.query_row("SELECT COUNT(*) FROM api_requests WHERE answer IS NULL", [], |r| r.get(0)).map_err(Error::from)?;
                if waiting >= 5 {
                    return Err(Refusal::new(-32002, "too many requests are waiting for the user"));
                }
                let kind = if params["plugin"].as_bool() == Some(true) { "plugin" } else { "program" };
                self.conn
                    .execute(
                        "INSERT INTO api_clients (name, publisher, kind, grants, created) VALUES (?1, ?2, ?3, '[]', ?4)",
                        params![name, publisher, kind, now()],
                    )
                    .map_err(Error::from)?;
                let client = self.conn.last_insert_rowid();
                self.conn
                    .execute(
                        "INSERT INTO api_requests (client, secret_hash, reason, asked, created) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![client, hash(&secret), reason, serde_json::to_string(&asked).map_err(Error::from)?, now()],
                    )
                    .map_err(Error::from)?;
                let request = self.conn.last_insert_rowid();
                self.api_log_line(Some(client), method, false, true, &format!("{name} ({publisher}) asks: {}", asked.iter().map(describe).collect::<Vec<_>>().join("; ")))?;
                Ok(json!({ "request": request, "state": "waiting for the user" }))
            }
            "v1.auth.collect" => {
                let request = params["request"].as_i64().ok_or_else(|| Refusal::new(-32602, "request: a number"))?;
                let secret = params["secret"].as_str().unwrap_or_default();
                let row: Option<(Vec<u8>, Option<String>, Option<String>)> = self
                    .conn
                    .query_row("SELECT secret_hash, answer, token FROM api_requests WHERE id = ?1", params![request], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .optional()
                    .map_err(Error::from)?;
                let Some((secret_hash, answer, token)) = row else { return Err(Refusal::new(-32602, "no such request")) };
                if secret_hash != hash(secret) {
                    return Err(Refusal::new(-32001, "that isn't this request's secret"));
                }
                match (answer.as_deref(), token) {
                    (None, _) => Ok(json!({ "state": "waiting for the user" })),
                    (Some("granted"), Some(token)) => {
                        // handed over once
                        self.conn.execute("UPDATE api_requests SET token = NULL WHERE id = ?1", params![request]).map_err(Error::from)?;
                        Ok(json!({ "state": "granted", "token": token }))
                    }
                    (Some("granted"), None) => Ok(json!({ "state": "granted", "token": null, "note": "the token was collected already" })),
                    _ => Ok(json!({ "state": "refused" })),
                }
            }
            _ => Err(Refusal::new(-32601, format!("there's no method {method}"))),
        }
    }

    /// Runs a granted method; returns its answer and what the log says it touched.
    fn run_method(&self, host: &dyn Host, client: &Client, scope: &Scope, method: &str, p: &Value) -> std::result::Result<(Value, String), Refusal> {
        let ids = |key: &str| -> std::result::Result<Vec<i64>, Refusal> {
            let list: Vec<i64> = p[key].as_array().map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
            if list.is_empty() || list.len() > 1000 {
                return Err(Refusal::new(-32602, format!("{key}: 1 to 1,000 message numbers")));
            }
            Ok(list)
        };
        let id = || p["id"].as_i64().ok_or_else(|| Refusal::new(-32602, "id: a message number"));
        let short = |list: &[i64]| -> String {
            let shown: Vec<String> = list.iter().take(20).map(i64::to_string).collect();
            if list.len() > 20 { format!("{} … ({} in all)", shown.join(", "), list.len()) } else { shown.join(", ") }
        };
        match method {
            "v1.accounts.list" => {
                let list: Vec<Value> = self
                    .accounts()?
                    .into_iter()
                    .filter(|a| scope.account_ok(a.address.as_deref()))
                    .map(|a| json!({ "id": a.id, "address": a.address, "name": a.label.clone().unwrap_or(a.name.clone()), "kind": a.kind_name, "messages": a.messages }))
                    .collect();
                let n = list.len();
                Ok((Value::Array(list), format!("{n} accounts")))
            }
            "v1.folders.list" => {
                let list: Vec<Value> = self
                    .folders()?
                    .into_iter()
                    .filter(|f| scope.folder_ok(f.id))
                    .map(|f| json!({ "id": f.id, "path": f.path, "account": f.account, "messages": f.messages }))
                    .collect();
                let n = list.len();
                Ok((Value::Array(list), format!("{n} folders")))
            }
            "v1.mail.list" => {
                let folder = p["folder"].as_i64().ok_or_else(|| Refusal::new(-32602, "folder: a folder number"))?;
                if !scope.folder_ok(folder) {
                    return Err(Refusal::new(-32001, "this program may not see that folder"));
                }
                let limit = p["limit"].as_u64().unwrap_or(100).clamp(1, 500) as usize;
                let after = match (p["after"]["date"].as_i64(), p["after"]["id"].as_i64()) {
                    (Some(date), Some(id)) => Some(crate::library::PageCursor { date, id }),
                    _ => None,
                };
                let rows = self.list_folder(folder, after, limit)?;
                let n = rows.len();
                Ok((Value::Array(rows.iter().map(row_json).collect()), format!("folder {folder}: {n} messages listed")))
            }
            "v1.mail.search" => {
                let query = p["query"].as_str().filter(|q| !q.trim().is_empty()).ok_or_else(|| Refusal::new(-32602, "query: what to search for"))?;
                let limit = p["limit"].as_u64().unwrap_or(100).clamp(1, 500) as usize;
                let (_, rows) = self.search_scoped(query, limit * 4, crate::index::SortOrder::Newest, p["everywhere"].as_bool().unwrap_or(false))?;
                let mut out = Vec::new();
                for r in rows {
                    if out.len() >= limit {
                        break;
                    }
                    if scope.message_ok(self, r.id)? && client.may(Permission::HeadersRead) {
                        out.push(row_json(&r));
                    }
                }
                let n = out.len();
                Ok((Value::Array(out), format!("searched \"{}\": {n} results", query.chars().take(80).collect::<String>())))
            }
            "v1.mail.headers" => {
                let id = id()?;
                scope.require(self, id)?;
                let v = self.message_view(id)?;
                let flags = self.flags_of(id)?;
                Ok((
                    json!({
                        "id": id, "subject": v.subject, "from": v.from, "to": v.to, "cc": v.cc, "date": v.date, "message_id": v.message_id,
                        "folders": self.folder_paths(id)?, "labels": self.labels_on(id)?, "unread": flags & crate::marks::UNREAD != 0,
                        "attachments": v.attachments.iter().map(|a| json!({ "part": a.part, "name": a.name, "mime": a.mime, "size": a.size })).collect::<Vec<_>>(),
                    }),
                    format!("headers of message {id}"),
                ))
            }
            "v1.mail.body" => {
                let id = id()?;
                scope.require(self, id)?;
                let v = self.message_view(id)?;
                Ok((json!({ "id": id, "text": v.text, "html": v.html }), format!("text of message {id}")))
            }
            "v1.mail.attachment" => {
                use base64::Engine;
                let id = id()?;
                scope.require(self, id)?;
                let part = p["part"].as_str().ok_or_else(|| Refusal::new(-32602, "part: the attachment's part"))?;
                let (mime, bytes) = self.attachment_bytes(id, part)?;
                let n = bytes.len();
                Ok((json!({ "mime": mime, "data": base64::engine::general_purpose::STANDARD.encode(bytes) }), format!("attachment {part} of message {id} ({n} bytes)")))
            }
            "v1.mail.move" | "v1.mail.delete" => {
                let list = ids("ids")?;
                let from = p["from"].as_i64().ok_or_else(|| Refusal::new(-32602, "from: the folder they're in"))?;
                if !scope.folder_ok(from) {
                    return Err(Refusal::new(-32001, "this program may not change that folder"));
                }
                for id in &list {
                    scope.require(self, *id)?;
                }
                let to = if method == "v1.mail.delete" {
                    let account: i64 = self.conn.query_row("SELECT account FROM folders WHERE id = ?1", params![from], |r| r.get(0)).map_err(Error::from)?;
                    self.trash_folder(account)?
                } else {
                    let to = p["to"].as_i64().ok_or_else(|| Refusal::new(-32602, "to: the folder to move them to"))?;
                    if !scope.folder_ok(to) {
                        return Err(Refusal::new(-32001, "this program may not file into that folder"));
                    }
                    to
                };
                let n = self.move_messages(&list, from, to)?;
                Ok((json!({ "moved": n }), format!("{} {n} messages from folder {from} to {to}: {}", if method == "v1.mail.delete" { "deleted" } else { "moved" }, short(&list))))
            }
            "v1.mail.mark" => {
                let list = ids("ids")?;
                for id in &list {
                    scope.require(self, *id)?;
                }
                let mut did = Vec::new();
                if let Some(s) = p["status"].as_str() {
                    let status = crate::marks::Status::parse(s).ok_or_else(|| Refusal::new(-32602, "status: unread, read, replied, forwarded or redirected"))?;
                    self.set_status(&list, status)?;
                    did.push(format!("status {s}"));
                }
                if let Some(level) = p["priority"].as_u64() {
                    self.set_priority(&list, (level > 0).then_some(level as u8))?;
                    did.push(format!("priority {level}"));
                }
                if let Some(label) = p["label"].as_str() {
                    self.set_label(&list, label, true)?;
                    did.push(format!("label {label}"));
                }
                if let Some(label) = p["unlabel"].as_str() {
                    self.set_label(&list, label, false)?;
                    did.push(format!("label {label} off"));
                }
                Ok((json!({ "changed": list.len() }), format!("{} on {}", did.join(", "), short(&list))))
            }
            "v1.mail.send" => {
                let draft: crate::compose::Draft = serde_json::from_value(p["draft"].clone()).map_err(|e| Refusal::new(-32602, format!("draft: {e}")))?;
                let from_ok = self.identities()?.iter().any(|i| i.can_send && i.address.eq_ignore_ascii_case(&draft.from));
                if !from_ok || !scope.account_ok(Some(&draft.from)) {
                    return Err(Refusal::new(-32001, format!("this program may not send as {}", draft.from)));
                }
                let to = format!("to {}: {}", draft.to, draft.subject.chars().take(80).collect::<String>());
                // Send Later: when it goes (unix seconds); now when not given
                let send_at = p["send_at"].as_i64().filter(|t| *t > now()).unwrap_or_else(now);
                if client.send_without_asking {
                    self.queue_message(&draft, send_at)?;
                    Ok((json!({ "state": "sent", "note": "queued in Out", "send_at": send_at }), format!("sent {to}")))
                } else {
                    let mut stored = serde_json::to_value(&draft).map_err(Error::from)?;
                    stored["send_at"] = send_at.into();
                    self.conn
                        .execute("INSERT INTO api_sends (client, draft, created) VALUES (?1, ?2, ?3)", params![client.id, stored.to_string(), now()])
                        .map_err(Error::from)?;
                    let send = self.conn.last_insert_rowid();
                    Ok((json!({ "state": "waiting for the user", "send": send }), format!("asked the user to send {to}")))
                }
            }
            "v1.mail.send_status" => {
                let send = p["send"].as_i64().ok_or_else(|| Refusal::new(-32602, "send: the number v1.mail.send gave"))?;
                let answer: Option<Option<String>> = self
                    .conn
                    .query_row("SELECT answer FROM api_sends WHERE id = ?1 AND client = ?2", params![send, client.id], |r| r.get(0))
                    .optional()
                    .map_err(Error::from)?;
                let state = match answer {
                    None => return Err(Refusal::new(-32602, "no such send from this program")),
                    Some(None) => "waiting for the user".to_string(),
                    Some(Some(a)) => a,
                };
                Ok((json!({ "state": state }), format!("asked about send {send}")))
            }
            "v1.rules.list" => {
                let rules = self.rules()?;
                let n = rules.len();
                Ok((serde_json::to_value(rules).map_err(Error::from)?, format!("{n} rules")))
            }
            "v1.rules.set" => {
                let rules: Vec<crate::rules::Rule> = serde_json::from_value(p["rules"].clone()).map_err(|e| Refusal::new(-32602, format!("rules: {e}")))?;
                let n = rules.len();
                self.set_rules(&rules)?;
                Ok((json!({ "rules": n }), format!("replaced the rules ({n})")))
            }
            _ => self.run_more(host, client, scope, method, p),
        }
    }
}

impl Client {
    pub(crate) fn may(&self, p: Permission) -> bool {
        self.grants.iter().any(|g| g.permission == p)
    }
}

fn describe(g: &Grant) -> String {
    let mut s = g.permission.plainly().to_string();
    if !g.accounts.is_empty() {
        s.push_str(&format!(" (accounts: {})", g.accounts.join(", ")));
    }
    if !g.folders.is_empty() {
        s.push_str(&format!(" (folders: {})", g.folders.join(", ")));
    }
    s
}

fn row_json(r: &crate::library::MessageRow) -> Value {
    json!({
        "id": r.id, "date": r.date, "from": r.from, "subject": r.subject, "size": r.size, "attachments": r.attachments,
        "message_id": r.message_id, "labels": r.labels, "unread": r.flags & crate::marks::UNREAD != 0,
    })
}

/// Where a granted permission reaches: some accounts and folders, or all.
pub(crate) struct Scope {
    /// Account ids allowed (None: every account).
    accounts: Option<Vec<i64>>,
    /// Folder ids allowed (None: every folder of the allowed accounts).
    folders: Option<Vec<i64>>,
    /// Account addresses allowed (for sending).
    addresses: Option<Vec<String>>,
    /// Each folder's account.
    folder_account: HashMap<i64, i64>,
}

impl Scope {
    fn of(library: &Library, client: &Client, p: Permission) -> Result<Scope> {
        let grant = client.grants.iter().find(|g| g.permission == p);
        let folder_account = Scope::folder_accounts(library)?;
        let Some(g) = grant else { return Ok(Scope { accounts: Some(Vec::new()), folders: Some(Vec::new()), addresses: Some(Vec::new()), folder_account }) };
        let accounts = if g.accounts.is_empty() {
            None
        } else {
            let mut ids = Vec::new();
            for a in library.accounts_counted(false)? {
                if a.address.as_deref().is_some_and(|x| g.accounts.iter().any(|w| w.eq_ignore_ascii_case(x))) {
                    ids.push(a.id);
                }
            }
            Some(ids)
        };
        let folders = if g.folders.is_empty() {
            None
        } else {
            let mut ids = Vec::new();
            for f in library.folders()? {
                let shown = crate::library::shown_path(&f.path);
                // a folder named, and every folder inside it
                if g.folders.iter().any(|w| shown.eq_ignore_ascii_case(w) || shown.to_lowercase().starts_with(&format!("{}/", w.to_lowercase()))) {
                    ids.push(f.id);
                }
            }
            Some(ids)
        };
        let addresses = (!g.accounts.is_empty()).then(|| g.accounts.clone());
        Ok(Scope { accounts, folders, addresses, folder_account })
    }

    fn folder_accounts(library: &Library) -> Result<HashMap<i64, i64>> {
        let mut stmt = library.conn.prepare("SELECT id, account FROM folders")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn folder_ok(&self, folder: i64) -> bool {
        let account_ok = match (&self.accounts, self.folder_account.get(&folder)) {
            (None, Some(_)) => true,
            (Some(ids), Some(a)) => ids.contains(a),
            (_, None) => false,
        };
        account_ok && self.folders.as_ref().is_none_or(|f| f.contains(&folder))
    }

    /// The grant reaches every account and folder (needed by what works on the whole library).
    pub(crate) fn whole(&self) -> bool {
        self.accounts.is_none() && self.folders.is_none()
    }

    pub(crate) fn account_id_ok(&self, account: i64) -> bool {
        self.accounts.as_ref().is_none_or(|a| a.contains(&account)) && self.folders.is_none()
    }

    pub(crate) fn account_ok(&self, address: Option<&str>) -> bool {
        match (&self.addresses, address) {
            (None, _) => true,
            (Some(list), Some(a)) => list.iter().any(|x| x.eq_ignore_ascii_case(a)),
            (Some(_), None) => false,
        }
    }

    /// A message is reachable when any folder it's in is.
    pub(crate) fn message_ok(&self, library: &Library, id: i64) -> Result<bool> {
        Ok(library.message_folders(id)?.into_iter().any(|f| self.folder_ok(f)))
    }

    pub(crate) fn require(&self, library: &Library, id: i64) -> std::result::Result<(), Refusal> {
        if self.message_ok(library, id)? { Ok(()) } else { Err(Refusal::new(-32001, format!("this program may not see message {id}"))) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;
    use crate::{ImportOptions, import};

    /// A library with two folders (Inbox, Bank), one message each; and a program granted
    /// headers and bodies in the Inbox only.
    fn setup() -> (tempfile::TempDir, Library, Limits, String, [i64; 2], [i64; 2]) {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        for (folder, n) in [("Inbox", 1), ("Bank", 2)] {
            let file = dir.path().join(format!("{n}.eml"));
            std::fs::write(&file, format!("From: ann@a.com\r\nTo: me@x.com\r\nSubject: Hello {n}\r\nMessage-ID: <{n}@a>\r\n\r\nAccount number 12345 in {folder}\r\n")).unwrap();
            import(&mut library, &[file], &ImportOptions { destination: Some(folder.into()), ..Default::default() }, &Silent).unwrap();
        }
        let folder = |p: &str| library.folders().unwrap().into_iter().find(|f| f.path.ends_with(p)).unwrap().id;
        let (inbox, bank) = (folder("Inbox"), folder("Bank"));
        let ids: Vec<i64> = library.conn.prepare("SELECT id FROM messages ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        let limits = Limits::default();
        let asked = json!({
            "name": "Invoice Exporter", "publisher": "Acme", "reason": "Exports invoices", "secret": "s3cret",
            "permissions": [{ "permission": "mail.headers.read" }, { "permission": "mail.bodies.read" }, { "permission": "mail.send" }],
        });
        let r = library.api_call(&limits, &NoHost, None, "v1.auth.request", &asked).unwrap();
        let request = r["request"].as_i64().unwrap();
        assert_eq!(library.api_call(&limits, &NoHost, None, "v1.auth.collect", &json!({ "request": request, "secret": "s3cret" })).unwrap()["state"], "waiting for the user");
        // the user grants less than asked: headers and bodies, Inbox only; no sending
        let path = library.folders().unwrap().into_iter().find(|f| f.id == inbox).unwrap().path;
        let grants = vec![
            Grant { permission: Permission::HeadersRead, accounts: vec![], folders: vec![crate::library::shown_path(&path).to_string()] },
            Grant { permission: Permission::BodiesRead, accounts: vec![], folders: vec![crate::library::shown_path(&path).to_string()] },
        ];
        library.api_answer(request, Some(&grants)).unwrap();
        assert!(library.api_call(&limits, &NoHost, None, "v1.auth.collect", &json!({ "request": request, "secret": "wrong" })).is_err());
        let got = library.api_call(&limits, &NoHost, None, "v1.auth.collect", &json!({ "request": request, "secret": "s3cret" })).unwrap();
        let token = got["token"].as_str().unwrap().to_string();
        assert!(library.api_call(&limits, &NoHost, None, "v1.auth.collect", &json!({ "request": request, "secret": "s3cret" })).unwrap()["token"].is_null(), "handed over once");
        (dir, library, limits, token, [inbox, bank], [ids[0], ids[1]])
    }

    #[test]
    fn a_program_reaches_only_what_the_user_granted() {
        let (_dir, library, limits, token, [inbox, bank], [in_inbox, in_bank]) = setup();
        let t = Some(token.as_str());
        let body = library.api_call(&limits, &NoHost, t, "v1.mail.body", &json!({ "id": in_inbox })).unwrap();
        assert!(body["text"].as_str().unwrap().contains("Inbox"));
        // the bank folder is out of its reach, by message and by folder
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.body", &json!({ "id": in_bank })).unwrap_err().code, -32001);
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.list", &json!({ "folder": bank })).unwrap_err().code, -32001);
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.list", &json!({ "folder": inbox })).unwrap().as_array().unwrap().len(), 1);
        // not granted at all: search, sending, deleting
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.search", &json!({ "query": "account" })).unwrap_err().code, -32001);
        let draft = json!({ "draft": { "from": "me@x.com", "to": "thief@evil.example", "subject": "numbers" } });
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.send", &draft).unwrap_err().code, -32001);
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.delete", &json!({ "ids": [in_inbox], "from": inbox })).unwrap_err().code, -32001);
        // no token, a wrong one, a revoked one
        assert_eq!(library.api_call(&limits, &NoHost, None, "v1.mail.body", &json!({ "id": in_inbox })).unwrap_err().code, -32001);
        assert_eq!(library.api_call(&limits, &NoHost, Some("eud1_guess"), "v1.mail.body", &json!({ "id": in_inbox })).unwrap_err().code, -32001);
        let client = library.api_clients().unwrap()[0].id;
        library.api_revoke(client).unwrap();
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.body", &json!({ "id": in_inbox })).unwrap_err().code, -32001);
        // and every step is in the log: reads, refusals, the grant, the revocation
        let log = library.api_log(Some(client), 100).unwrap();
        assert!(log.iter().any(|l| l.method == "v1.mail.body" && l.allowed && l.detail.contains(&in_inbox.to_string())));
        assert!(log.iter().any(|l| l.method == "v1.mail.send" && !l.allowed));
        assert!(log.iter().any(|l| l.method == "grant"));
        assert!(log.iter().any(|l| l.method == "revoke"));
    }

    #[test]
    fn sending_waits_for_the_user_until_trusted_and_the_log_can_be_turned_down() {
        let (_dir, library, limits, token, _, [in_inbox, _]) = setup();
        let client = library.api_clients().unwrap()[0].id;
        // grant sending too (the user changes their mind)
        let mut grants = library.api_clients().unwrap()[0].grants.clone();
        grants.push(Grant { permission: Permission::Send, accounts: vec![], folders: vec![] });
        library.conn.execute("UPDATE api_clients SET grants = ?2 WHERE id = ?1", params![client, serde_json::to_string(&grants).unwrap()]).unwrap();
        library.conn.execute("UPDATE accounts SET kind = 'graph', address = 'me@x.com'", []).unwrap();
        let t = Some(token.as_str());
        let draft = json!({ "draft": { "from": "me@x.com", "to": "bob@y.com", "subject": "Invoice" } });
        let r = library.api_call(&limits, &NoHost, t, "v1.mail.send", &draft).unwrap();
        assert_eq!(r["state"], "waiting for the user");
        assert!(library.outbox().unwrap().is_empty(), "nothing goes before the user says so");
        let pending = library.api_pending_sends().unwrap();
        assert_eq!(pending.len(), 1);
        // OK, and "allow all from this program"
        library.api_answer_send(pending[0].id, true, true).unwrap();
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.send_status", &json!({ "send": pending[0].id })).unwrap()["state"], "sent");
        assert_eq!(library.outbox().unwrap().len(), 1);
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.send", &draft).unwrap()["state"], "sent", "trusted: goes at once");
        // sending as an address it may not use
        let other = json!({ "draft": { "from": "boss@x.com", "to": "bob@y.com", "subject": "x" } });
        assert_eq!(library.api_call(&limits, &NoHost, t, "v1.mail.send", &other).unwrap_err().code, -32001);

        // trusted: reads no longer logged, writes and refusals still are
        library.api_set_trust(client, LogLevel::Changes, true).unwrap();
        let before = library.api_log(Some(client), 1000).unwrap().len();
        library.api_call(&limits, &NoHost, t, "v1.mail.body", &json!({ "id": in_inbox })).unwrap();
        assert_eq!(library.api_log(Some(client), 1000).unwrap().len(), before, "a read isn't logged at 'changes'");
        library.api_call(&limits, &NoHost, t, "v1.mail.send", &draft).unwrap();
        assert_eq!(library.api_log(Some(client), 1000).unwrap().len(), before + 1, "a send is");
    }

    #[test]
    fn rate_limits_slow_a_program_that_reads_everything() {
        let limits = Limits::default();
        for _ in 0..1000 {
            assert!(limits.allow(1, Permission::BodiesRead, 1));
        }
        assert!(!limits.allow(1, Permission::BodiesRead, 1), "the 1,001st body in a minute waits");
        assert!(limits.allow(2, Permission::BodiesRead, 1), "each program has its own budget");
        assert!(limits.allow(1, Permission::HeadersRead, 1), "and each kind of call");
    }

    #[test]
    fn every_method_is_documented_with_its_permission() {
        for m in all_methods() {
            assert!(m.name.starts_with("v1."), "{}", m.name);
            assert!(!m.summary.is_empty() && !m.params.is_empty());
            if m.write {
                assert!(m.permission.is_some(), "{} changes things: it needs a permission", m.name);
            }
        }
        // nothing reads a secret
        assert!(!all_methods().any(|m| m.name.contains("phrase") || m.name.contains("token.") || m.name.contains("api.answer") || m.name.contains("api.grant")));
    }
}
