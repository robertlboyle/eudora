//! IMAP and POP3 accounts ("mail server accounts"): their settings, signing in (passwords in
//! the computer's credential store, never in the library), testing a connection, reading
//! folders and messages for the download (which runs them next to the Microsoft 365 mailboxes;
//! see `graph_sync`), and sending through the account's SMTP server.
//!
//! Downloading changes nothing on the server: IMAP folders are opened with EXAMINE and messages
//! read with BODY.PEEK (nothing marked read). Eudora's own changes go to IMAP servers through
//! `imap_changes` (unless the account says not to); POP3 messages are deleted only when the
//! account says so, some days after Eudora has them. Server ids:
//! "<UIDVALIDITY>:<UID>" for IMAP (a folder renumbered by its server is read again),
//! "pop:<UIDL>" for POP3.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImapConfig {
    /// The account's email address (also how it's shown, unless given a name).
    pub address: String,
    /// The person's name, for the From line.
    pub name: String,
    /// "imap" (also when empty) or "pop3".
    pub protocol: String,
    pub server: String,
    pub port: u16,
    /// "tls" (993 / 995), "starttls" (143 / 110) or "none".
    pub security: String,
    /// Encryption level: "high" (also when empty), "strict" or "compatible" (`TlsLevel`).
    pub tls_level: String,
    pub username: String,
    /// Sending: the SMTP server (empty: this account doesn't send).
    pub smtp_server: String,
    pub smtp_port: u16,
    /// "starttls" (587), "tls" (465) or "none".
    pub smtp_security: String,
    /// Encryption level for sending, like `tls_level`.
    pub smtp_tls_level: String,
    /// Empty: the same user name as for reading mail.
    pub smtp_username: String,
    /// Sign in to send (most servers need it; None = yes).
    pub smtp_auth: Option<bool>,
    /// POP3: delete a message from the server this many days after Eudora has it (None =
    /// leave it there).
    pub pop_delete_after_days: Option<u32>,
    /// POP3: leave messages bigger than this many MB on the server, not downloaded (None = any
    /// size). They're listed in the download's report.
    pub pop_skip_over_mb: Option<u32>,
    /// IMAP: Eudora's changes (moves, read, labels, deletes, new folders) are made on the
    /// server too. Off: they stay in this library, as before.
    pub server_writes: Option<bool>,
}

impl ImapConfig {
    pub fn is_pop3(&self) -> bool {
        self.protocol.eq_ignore_ascii_case("pop3")
    }

    pub fn kind(&self) -> &'static str {
        if self.is_pop3() { "pop3" } else { "imap" }
    }

    pub fn can_send(&self) -> bool {
        !self.smtp_server.trim().is_empty()
    }

    /// Whether Eudora changes mail on this account's server (IMAP only: POP3 has no folders,
    /// flags or moves).
    pub fn writes(&self) -> bool {
        !self.is_pop3() && self.server_writes != Some(false)
    }

    fn user(&self) -> &str {
        if self.username.trim().is_empty() { self.address.trim() } else { self.username.trim() }
    }

    fn security(&self) -> &str {
        if self.security.is_empty() { "tls" } else { &self.security }
    }

    fn port(&self) -> u16 {
        match (self.port, self.is_pop3(), self.security()) {
            (p, _, _) if p != 0 => p,
            (_, false, "tls") => 993,
            (_, false, _) => 143,
            (_, true, "tls") => 995,
            (_, true, _) => 110,
        }
    }
}

fn secret_name(address: &str) -> String {
    format!("imap:{}", address.to_lowercase())
}

fn smtp_secret_name(address: &str) -> String {
    format!("smtp:{}", address.to_lowercase())
}

/// Keeps the password in this computer's credential store.
pub fn remember_password(address: &str, password: &str) -> Result<()> {
    crate::graph::store_secret(&secret_name(address), password.as_bytes())
}

fn password(address: &str) -> Result<String> {
    crate::graph::load_secret(&secret_name(address))
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| Error::Other(format!("the password for {address} isn't on this computer: enter it in Tools ▸ Personalities ▸ Change…")))
}

/// The password for sending: its own if one was given, else the account's.
fn smtp_password(address: &str) -> Result<String> {
    match crate::graph::load_secret(&smtp_secret_name(address)).and_then(|b| String::from_utf8(b).ok()) {
        Some(p) => Ok(p),
        None => password(address),
    }
}

pub type Session = imap::Session<imap::Connection>;

fn imap_error(what: &str, e: imap::Error) -> Error {
    match e {
        imap::Error::Io(io) => Error::Network(format!("{what}: {io}")),
        other => Error::Other(format!("{what}: {other}")),
    }
}

/// Signs in to an IMAP server over Eudora's own connection (`mailnet`: the account's
/// encryption level, STARTTLS done here), handed to the IMAP library. Returns the session and
/// the encryption agreed.
/// A session on an account's IMAP server, signed in with its password (sync's folder).
pub fn open_session(config: &ImapConfig) -> Result<Session> {
    connect_imap(config, &password(&config.address)?).map(|(s, _)| s)
}

fn connect_imap(config: &ImapConfig, password: &str) -> Result<(Session, String)> {
    use crate::mailnet::{Conn, TlsLevel, with_level};
    let host = config.server.trim();
    let level = TlsLevel::from_name(&config.tls_level);
    let (stream, security, greeting_read) = with_level(level, host, |engine| {
        let mut conn = Conn::open(host, config.port(), config.security() == "tls", engine)?;
        if config.security() != "starttls" {
            return Ok((conn, false));
        }
        let greeting = conn.line()?;
        if !greeting.starts_with("* OK") {
            return Err(Error::Other(format!("{host} didn't greet as an IMAP server: {greeting}")));
        }
        conn.command("a0 STARTTLS")?;
        loop {
            let line = conn.line()?;
            if let Some(answer) = line.strip_prefix("a0 ") {
                if !answer.to_ascii_uppercase().starts_with("OK") {
                    return Err(Error::Other(format!("{host} doesn't offer STARTTLS on port {} ({answer}): try SSL/TLS (port 993)", config.port())));
                }
                break;
            }
        }
        conn.start_tls(engine)?;
        Ok((conn, true))
    })
    .map(|(conn, greeted)| {
        let (stream, security) = conn.into_imap();
        (stream, security, greeted)
    })?;
    let mut client = imap::Client::new(Box::new(stream) as imap::Connection);
    if greeting_read {
        client.greeting_read = true;
    } else {
        client.read_greeting().map_err(|e| imap_error(&format!("connecting to {host}"), e))?;
    }
    let session = client.login(config.user(), password).map_err(|(e, _)| match e {
        imap::Error::No(_) | imap::Error::Bad(_) => Error::Other(format!("{host} refused the user name or password")),
        other => imap_error("signing in", other),
    })?;
    Ok((session, security))
}

/// A signed-in connection to an account's mail server.
pub enum Connection {
    /// The session and the encryption agreed.
    Imap(Box<Session>, String),
    Pop(crate::pop3::Pop3),
}

/// One message on the server: its id in the library and how to ask the server for it.
pub struct Item {
    pub id: String,
    key: u32,
    /// Its size, when the server says (POP3).
    pub size: Option<u64>,
}

impl Connection {
    /// Signs in (with `password`, or the one kept on this computer).
    pub fn open(config: &ImapConfig, password_given: Option<&str>) -> Result<Connection> {
        if config.server.trim().is_empty() {
            return Err(Error::Other("the mail server's name is missing".into()));
        }
        let password = match password_given {
            Some(p) => p.to_string(),
            None => password(&config.address)?,
        };
        if config.is_pop3() {
            let level = crate::mailnet::TlsLevel::from_name(&config.tls_level);
            Ok(Connection::Pop(crate::pop3::Pop3::open(config.server.trim(), config.port(), config.security(), level, config.user(), &password)?))
        } else {
            let (session, security) = connect_imap(config, &password)?;
            Ok(Connection::Imap(Box::new(session), security))
        }
    }

    /// The encryption agreed with the server ("TLS 1.3, TLS13_AES_256_GCM_SHA384").
    pub fn security(&self) -> &str {
        match self {
            Connection::Imap(_, security) => security,
            Connection::Pop(pop) => pop.security(),
        }
    }

    /// Every folder that can hold mail, with its message count (POP3: just the Inbox).
    pub fn folders(&mut self) -> Result<Vec<Folder>> {
        match self {
            Connection::Imap(session, _) => folders(session),
            Connection::Pop(pop) => Ok(vec![Folder { server_name: "INBOX".into(), path: "Inbox".into(), messages: pop.count()? }]),
        }
    }

    /// The folder's messages, oldest first.
    pub fn list(&mut self, folder: &str) -> Result<Vec<Item>> {
        self.list_since(folder, None)
    }

    /// `list`, only messages the server received since `since` (unix seconds, by the day: IMAP's
    /// SINCE) when given. POP3 has no dates: everything.
    pub fn list_since(&mut self, folder: &str, since: Option<i64>) -> Result<Vec<Item>> {
        match self {
            Connection::Imap(session, _) => {
                let (validity, uids) = uids_since(session, folder, since)?;
                Ok(uids.into_iter().map(|uid| Item { id: format!("{validity}:{uid}"), key: uid, size: None }).collect())
            }
            Connection::Pop(pop) => {
                // (sizes for skipping big messages; a server without LIST still lists them)
                let sizes: std::collections::HashMap<u32, u64> = pop.sizes().map(|s| s.into_iter().collect()).unwrap_or_default();
                Ok(pop.uidl()?.into_iter().map(|(n, uidl)| Item { id: format!("pop:{uidl}"), key: n, size: sizes.get(&n).copied() }).collect())
            }
        }
    }

    /// Reads these messages of the folder listed last: (id, bytes, the server's date for it).
    pub fn fetch(&mut self, items: &[&Item]) -> Result<Vec<(String, Vec<u8>, Option<i64>)>> {
        match self {
            Connection::Imap(session, _) => {
                let by_uid: std::collections::HashMap<u32, &str> = items.iter().map(|i| (i.key, i.id.as_str())).collect();
                let uids: Vec<u32> = items.iter().map(|i| i.key).collect();
                Ok(fetch(session, &uids)?
                    .into_iter()
                    .filter_map(|(uid, raw, date)| by_uid.get(&uid).map(|id| (id.to_string(), raw, date)))
                    .collect())
            }
            Connection::Pop(pop) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push((item.id.clone(), pop.retr(item.key)?, None));
                }
                Ok(out)
            }
        }
    }

    /// POP3: deletes these messages from the server (at QUIT, so the connection is ended).
    /// Returns how many. IMAP: nothing (its deletes go through `imap_changes`).
    pub fn delete_and_quit(self, items: &[&Item]) -> Result<u64> {
        match self {
            Connection::Pop(mut pop) => {
                for item in items {
                    pop.dele(item.key)?;
                }
                pop.quit();
                Ok(items.len() as u64)
            }
            other => {
                other.logout();
                Ok(0)
            }
        }
    }

    pub fn logout(self) {
        match self {
            Connection::Imap(mut session, _) => {
                let _ = session.logout();
            }
            Connection::Pop(pop) => pop.quit(),
        }
    }
}

/// A folder of an IMAP account.
#[derive(Clone, Debug, Serialize)]
pub struct Folder {
    /// The server's name for it (as sent: modified UTF-7).
    pub server_name: String,
    /// As shown, with "/" between levels ("Projects/2026").
    pub path: String,
    pub messages: u32,
}

/// Every folder that can hold mail, with its message count.
pub fn folders(session: &mut Session) -> Result<Vec<Folder>> {
    let names = session.list(Some(""), Some("*")).map_err(|e| imap_error("listing the folders", e))?;
    let mut out = Vec::new();
    for n in names.iter() {
        if n.attributes().iter().any(|a| format!("{a:?}") == "NoSelect") {
            continue;
        }
        // (sync's own folder: its items are settings, never mail)
        if crate::sync_stores::is_sync_folder(&decode_utf7(n.name())) {
            continue;
        }
        let server_name = n.name().to_string();
        let delimiter = n.delimiter().unwrap_or("/");
        let path = decode_utf7(&server_name).split(delimiter).map(|p| p.replace('/', "-")).collect::<Vec<_>>().join("/");
        let messages = session.status(&server_name, "(MESSAGES)").ok().map(|m| m.exists).unwrap_or(0);
        out.push(Folder { server_name, path, messages });
    }
    out.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    Ok(out)
}

/// Opens a folder read-only: its UIDVALIDITY and every message's UID (newest last).
pub fn uids(session: &mut Session, folder: &str) -> Result<(u32, Vec<u32>)> {
    uids_since(session, folder, None)
}

/// `uids`, only messages received since `since` (unix seconds) when given.
pub fn uids_since(session: &mut Session, folder: &str, since: Option<i64>) -> Result<(u32, Vec<u32>)> {
    let mailbox = session.examine(folder).map_err(|e| imap_error(&format!("opening {folder}"), e))?;
    let validity = mailbox.uid_validity.unwrap_or(0);
    if mailbox.exists == 0 {
        return Ok((validity, Vec::new()));
    }
    let query = match since.and_then(|t| chrono::DateTime::from_timestamp(t, 0)) {
        Some(t) => format!("SINCE {}", t.format("%-d-%b-%Y")),
        None => "ALL".to_string(),
    };
    let set: HashSet<u32> = session.uid_search(&query).map_err(|e| imap_error(&format!("listing {folder}"), e))?;
    let mut uids: Vec<u32> = set.into_iter().collect();
    uids.sort_unstable();
    Ok((validity, uids))
}

/// Reads messages of the folder opened last (`uids`), each as stored: (uid, bytes, the
/// server's date for it, unix seconds).
pub fn fetch(session: &mut Session, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>, Option<i64>)>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let fetches = session.uid_fetch(&set, "(UID INTERNALDATE BODY.PEEK[])").map_err(|e| imap_error("reading messages", e))?;
    Ok(fetches
        .iter()
        .filter_map(|f| Some((f.uid?, f.body()?.to_vec(), f.internal_date().map(|d| d.timestamp()))))
        .collect())
}

/// One message as an IMAP server has it (for looking at an account: `eudora-cli imap show`).
#[derive(Clone, Debug, Serialize)]
pub struct ServerMessage {
    pub uid: u32,
    pub flags: Vec<String>,
    pub size: u32,
    pub subject: String,
    pub message_id: String,
}

/// An IMAP account's folders as its server lists them (read-only).
pub fn server_folders(config: &ImapConfig) -> Result<Vec<Folder>> {
    match Connection::open(config, None)? {
        Connection::Imap(mut session, _) => {
            let out = folders(&mut session);
            let _ = session.logout();
            out
        }
        other => {
            other.logout();
            Err(Error::Other("a POP3 account has no folders to look at".into()))
        }
    }
}

/// The newest `limit` messages of a folder (its server name or Eudora's path), with their
/// flags, size, subject and Message-ID; opened read-only (EXAMINE, BODY.PEEK): nothing changes.
/// Returns the folder's UIDVALIDITY too.
pub fn peek(config: &ImapConfig, folder: &str, limit: usize) -> Result<(u32, Vec<ServerMessage>)> {
    let Connection::Imap(mut session, _) = Connection::open(config, None)? else {
        return Err(Error::Other("a POP3 account has no folders to look at".into()));
    };
    let name = folders(&mut session)?.into_iter().find(|f| f.server_name == folder || f.path.eq_ignore_ascii_case(folder)).map_or_else(|| folder.to_string(), |f| f.server_name);
    let (validity, uids) = uids(&mut session, &name)?;
    let newest: Vec<String> = uids.iter().rev().take(limit).map(u32::to_string).collect();
    let mut out = Vec::new();
    if !newest.is_empty() {
        let fetches = session.uid_fetch(newest.join(","), "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])").map_err(|e| imap_error("reading messages", e))?;
        for f in fetches.iter() {
            let headers = crate::header_index::read_headers(f.header().unwrap_or_default());
            let get = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone()).unwrap_or_default();
            out.push(ServerMessage {
                uid: f.uid.unwrap_or(0),
                flags: f.flags().iter().map(|fl| fl.to_string()).collect(),
                size: f.size.unwrap_or(0),
                subject: get("Subject"),
                message_id: get("Message-ID"),
            });
        }
    }
    out.sort_by(|a, b| b.uid.cmp(&a.uid));
    let _ = session.logout();
    Ok((validity, out))
}

/// IMAP's "modified UTF-7" folder names (RFC 3501 5.1.3: "&AOk-" is "é", "&-" is "&").
pub fn decode_utf7(name: &str) -> String {
    use base64::Engine as _;
    let mut out = String::new();
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('-') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let encoded = &after[..end];
        if encoded.is_empty() {
            out.push('&');
        } else {
            let b64 = encoded.replace(',', "/");
            match base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64.trim_end_matches('=')) {
                Ok(bytes) => {
                    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                    out.push_str(&String::from_utf16_lossy(&units));
                }
                Err(_) => {
                    out.push('&');
                    out.push_str(encoded);
                    out.push('-');
                }
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

// ------------------------------------------------------------------ sending

fn smtp_settings<'a>(config: &'a ImapConfig, password: &'a str) -> crate::smtp::SmtpSettings<'a> {
    let security = if config.smtp_security.is_empty() { "starttls" } else { config.smtp_security.as_str() };
    let port = match (config.smtp_port, security) {
        (p, _) if p != 0 => p,
        (_, "tls") => 465,
        _ => 587,
    };
    let user = if config.smtp_auth == Some(false) {
        ""
    } else if config.smtp_username.trim().is_empty() {
        config.user()
    } else {
        config.smtp_username.trim()
    };
    let level = crate::mailnet::TlsLevel::from_name(&config.smtp_tls_level);
    crate::smtp::SmtpSettings { host: config.smtp_server.trim(), port, security, user, password, level }
}

/// What happened to the sent copy.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum SentCopy {
    /// Put in the server's Sent folder (IMAP): the download brings it in.
    OnServer(String),
    /// Kept in the library's Sent folder for this account (POP3, or IMAP when the server's
    /// Sent folder couldn't be used).
    InLibrary,
    /// Sent, but the copy couldn't be kept (why).
    NotKept(String),
}

/// Sends `message` (built by `compose::build_mime`) through the account's SMTP server, then
/// keeps a copy in its Sent folder (IMAP: APPEND on the server; POP3: `spool_sent`).
pub fn send(config: &ImapConfig, library_root: &std::path::Path, account: i64, recipients: &[String], message: &[u8]) -> Result<SentCopy> {
    if !config.can_send() {
        return Err(Error::Other(format!("{} has no outgoing (SMTP) server: add it in Tools ▸ Personalities ▸ Change…", config.address)));
    }
    let password = smtp_password(&config.address)?;
    crate::smtp::send(&smtp_settings(config, &password), &config.address, recipients, message)?;
    if config.is_pop3() {
        return Ok(match spool_sent(library_root, account, message) {
            Ok(()) => SentCopy::InLibrary,
            Err(e) => SentCopy::NotKept(e.to_string()),
        });
    }
    Ok(match append_to_sent(config, message) {
        Ok(folder) => {
            let _ = crate::graph_sync::request_check(library_root);
            SentCopy::OnServer(folder)
        }
        Err(e) => match spool_sent(library_root, account, message) {
            Ok(()) => SentCopy::InLibrary,
            Err(_) => SentCopy::NotKept(e.to_string()),
        },
    })
}

/// The account's Sent folder on an IMAP server (its \Sent mark, else a usual name), made if
/// there's none; the message is added there marked read.
fn append_to_sent(config: &ImapConfig, message: &[u8]) -> Result<String> {
    let (mut session, _) = connect_imap(config, &password(&config.address)?)?;
    let names = session.list(Some(""), Some("*")).map_err(|e| imap_error("listing the folders", e))?;
    let marked = names.iter().find(|n| n.attributes().iter().any(|a| format!("{a:?}").contains("Sent"))).map(|n| n.name().to_string());
    let usual = ["Sent", "Sent Items", "Sent Messages", "INBOX.Sent", "INBOX/Sent", "[Gmail]/Sent Mail"];
    let named = names.iter().find(|n| usual.iter().any(|u| n.name().eq_ignore_ascii_case(u))).map(|n| n.name().to_string());
    drop(names);
    let folder = match marked.or(named) {
        Some(f) => f,
        None => {
            session.create("Sent").map_err(|e| imap_error("making the Sent folder", e))?;
            "Sent".to_string()
        }
    };
    session
        .append(&folder, message)
        .flag(imap::types::Flag::Seen)
        .finish()
        .map_err(|e| imap_error(&format!("putting the copy in {folder}"), e))?;
    let _ = session.logout();
    Ok(decode_utf7(&folder))
}

/// Where sent copies wait for the download to put them in the library ("<account>-….eml").
pub const SENT_SPOOL: &str = "spool/sent";

/// Leaves a sent copy for the download (the library's only writer) to file in the account's
/// Sent folder.
pub fn spool_sent(library_root: &std::path::Path, account: i64, message: &[u8]) -> Result<()> {
    use crate::error::IoContext;
    let dir = library_root.join(SENT_SPOOL);
    std::fs::create_dir_all(&dir).context(|| format!("creating {}", dir.display()))?;
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    let name = format!("{account}-{}-{}.eml", crate::library::now(), random.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let part = dir.join(format!("{name}.part"));
    std::fs::write(&part, message).context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, dir.join(&name)).context(|| format!("naming {name}"))?;
    let _ = crate::graph_sync::request_check(library_root);
    Ok(())
}

// ------------------------------------------------------------------ testing

/// What the account window's Test shows.
#[derive(Clone, Debug, Serialize)]
pub struct TestResult {
    pub folders: Vec<Folder>,
    pub messages: u64,
    /// The encryption agreed for incoming mail ("TLS 1.3, …", "not encrypted").
    pub security: String,
    /// An SMTP server was given and tried.
    pub smtp_tested: bool,
    /// The encryption agreed with it, when signing in worked.
    pub smtp_security: Option<String>,
    /// Why signing in to it didn't work (None: it did).
    pub smtp_error: Option<String>,
}

/// Signs in with these settings, lists the folders and signs in to the SMTP server (nothing
/// is saved or sent). `smtp_password`: empty = the same password.
pub fn test(config: &ImapConfig, password: &str, smtp_password: &str) -> Result<TestResult> {
    let mut connection = Connection::open(config, Some(password))?;
    let security = connection.security().to_string();
    let folders = connection.folders()?;
    connection.logout();
    let p = if smtp_password.is_empty() { password } else { smtp_password };
    let (smtp_security, smtp_error) = if config.can_send() {
        match crate::smtp::test(&smtp_settings(config, p)) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e.to_string())),
        }
    } else {
        (None, None)
    };
    Ok(TestResult { messages: folders.iter().map(|f| f.messages as u64).sum(), folders, security, smtp_tested: config.can_send(), smtp_security, smtp_error })
}

const SETTINGS_PREFIX: &str = "imap-account:";

impl crate::Library {
    /// Adds (or updates) a mail server account: its settings in the library, the passwords on
    /// this computer. Returns the account.
    pub fn save_imap_account(&self, config: &ImapConfig, password: Option<&str>, smtp_password: Option<&str>) -> Result<i64> {
        if !config.address.contains('@') {
            return Err(Error::Other("the account's email address is missing".into()));
        }
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            remember_password(&config.address, p)?;
        }
        if let Some(p) = smtp_password.filter(|p| !p.is_empty()) {
            crate::graph::store_secret(&smtp_secret_name(&config.address), p.as_bytes())?;
        }
        let id = match self.imap_account_id(&config.address)? {
            // (the kind stays as it was made: IMAP and POP3 copies would be different folders)
            Some(id) => {
                self.conn.execute("UPDATE accounts SET name = ?2 WHERE id = ?1", rusqlite::params![id, config.name])?;
                id
            }
            None => crate::db::import_account(&self.conn, config.kind(), Some(&config.address), &config.name, &format!("{}:{}", config.server, config.port()), false, crate::library::now())?,
        };
        self.set_setting(&format!("{SETTINGS_PREFIX}{}", config.address.to_lowercase()), &serde_json::to_string(config)?)?;
        Ok(id)
    }

    /// The IMAP / POP3 accounts' settings.
    pub fn imap_accounts(&self) -> Result<Vec<ImapConfig>> {
        let mut stmt = self.conn.prepare("SELECT value FROM settings_items WHERE key LIKE ?1 || '%' ORDER BY key")?;
        let rows: Vec<String> = stmt.query_map([SETTINGS_PREFIX], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows.iter().filter_map(|j| serde_json::from_str(j).ok()).collect())
    }

    /// The IMAP accounts whose servers Eudora changes (moves, marks, deletes): their ids.
    pub fn imap_writable(&self) -> Result<std::collections::HashSet<i64>> {
        let mut out = std::collections::HashSet::new();
        for config in self.imap_accounts()?.into_iter().filter(ImapConfig::writes) {
            if let Some(id) = self.imap_account_id(&config.address)? {
                out.insert(id);
            }
        }
        Ok(out)
    }

    /// The library's account for a mail server account's address.
    pub fn imap_account_id(&self, address: &str) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT id FROM accounts WHERE kind IN ('imap', 'pop3') AND lower(address) = lower(?1)", [address], |r| r.get(0))
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_utf7_folder_names() {
        assert_eq!(decode_utf7("INBOX"), "INBOX");
        assert_eq!(decode_utf7("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode_utf7("&ZeVnLIqe-"), "日本語");
        assert_eq!(decode_utf7("Tom &- Jerry"), "Tom & Jerry");
    }

    #[test]
    fn ports_follow_the_protocol_and_security() {
        let imap = ImapConfig { security: "tls".into(), ..Default::default() };
        assert_eq!(imap.port(), 993);
        let pop = ImapConfig { protocol: "pop3".into(), security: "starttls".into(), ..Default::default() };
        assert_eq!((pop.port(), pop.kind()), (110, "pop3"));
        let smtp = ImapConfig { smtp_server: "smtp.x".into(), smtp_security: "tls".into(), ..Default::default() };
        assert_eq!(smtp_settings(&smtp, "p").port, 465);
        assert_eq!(smtp_settings(&ImapConfig::default(), "p").port, 587);
    }

    #[test]
    fn imap_accounts_are_kept_in_the_library() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let library = crate::Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let config = ImapConfig {
            address: "me@example.com".into(),
            name: "Me".into(),
            server: "imap.example.com".into(),
            port: 993,
            security: "tls".into(),
            username: "me".into(),
            ..Default::default()
        };
        // (no password: the test never touches the credential store)
        let id = library.save_imap_account(&config, None, None).unwrap();
        assert_eq!(library.imap_accounts().unwrap(), vec![config.clone()]);
        assert_eq!(library.save_imap_account(&config, None, None).unwrap(), id, "saving again updates it");
        assert_eq!(library.imap_account_id("ME@example.com").unwrap(), Some(id));
        let a = library.accounts().unwrap().into_iter().find(|a| a.id == id).unwrap();
        assert_eq!((a.kind_name, a.address.as_deref()), ("IMAP", Some("me@example.com")));
        let pop = ImapConfig { address: "you@example.com".into(), protocol: "pop3".into(), server: "pop.example.com".into(), ..Default::default() };
        let pop_id = library.save_imap_account(&pop, None, None).unwrap();
        let a = library.accounts().unwrap().into_iter().find(|a| a.id == pop_id).unwrap();
        assert_eq!(a.kind, "pop3");
        assert!(test(&ImapConfig { server: String::new(), ..config }, "x", "").is_err());
    }
}
