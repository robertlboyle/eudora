//! What the user's programs share for reading and writing mail (the desktop viewer and the web
//! interface): a message as shown, the addresses the user can write from, how a new message,
//! reply or forward starts, and sending it through the right account.
//!
//! Sending is split in two so a caller holding the library behind a lock can let go of it
//! during the network part: `Library::prepare_send` (reads the library) → `Outgoing::send`
//! (talks to Microsoft 365 or the SMTP server, no library).

use serde::Serialize;

use crate::compose::{self, Draft, Seed};
use crate::error::{Error, Result};
use crate::parse::Addr;
use crate::Library;

#[derive(Clone, Debug, Serialize)]
pub struct Person {
    pub name: String,
    pub address: String,
}

fn people(list: &[Addr]) -> Vec<Person> {
    list.iter().map(|a| Person { name: a.name.clone(), address: a.address.clone() }).collect()
}

#[derive(Clone, Debug, Serialize)]
pub struct AttachmentView {
    /// Where it is in the message (for fetching it: `attachment_bytes`).
    pub part: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// A classic Eudora attachment whose file wasn't found: the path the message wrote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
    /// A picture the message shows itself (its HTML names it by `cid:`): left out of the
    /// attachments while the message is shown, listed with the other views.
    pub inline: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CopyView {
    pub id: i64,
    pub format: String,
    pub size: u64,
    /// "classic Eudora › OldOut.fol/Out1999"
    pub from: String,
}

/// A message as shown.
#[derive(Clone, Debug, Serialize)]
pub struct MessageView {
    pub id: i64,
    /// A payment's receipt, or a receipt's payment (same amount, within 30 minutes).
    pub linked: Vec<crate::links::Linked>,
    /// A meeting is in it (an invitation, an update, a cancellation): the window shows its bar.
    pub calendar: bool,
    pub subject: String,
    pub from: Vec<Person>,
    pub to: Vec<Person>,
    pub cc: Vec<Person>,
    pub bcc: Vec<Person>,
    pub date: i64,
    pub message_id: String,
    pub message_id_made: bool,
    /// The HTML body: not cleaned, so show it only where no script runs and nothing loads from
    /// the network (a sandboxed frame with a strict content policy).
    pub html: Option<String>,
    pub text: String,
    pub headers: String,
    pub attachments: Vec<AttachmentView>,
    pub folders: Vec<String>,
    /// The folders it's in, by id (a message window acts on it there: Spam, Delete, Transfer…).
    pub folder_ids: Vec<i64>,
    pub size: u64,
    /// What the parser had to fix or guess.
    pub problems: Vec<String>,
    /// Every original copy kept (each source's own bytes).
    pub copies: Vec<CopyView>,
    /// The account it belongs to (its picture settings apply).
    pub account: Option<i64>,
    /// Pictures from the web its HTML names (as written there).
    pub web_pictures: Vec<String>,
    /// A fraud warning, shown above the message (`phishing::warning`).
    pub warning: Option<String>,
}

/// An address the user can write from.
#[derive(Clone, Debug, Serialize)]
pub struct Identity {
    pub address: String,
    pub name: String,
    /// "Microsoft 365", "Microsoft 365 (shared)", "IMAP", "POP3", "classic Eudora personality"
    pub kind: String,
    /// Some account can send as it.
    pub can_send: bool,
}

/// How a new message starts, and who it can be from.
#[derive(Clone, Debug, Serialize)]
pub struct ComposeStart {
    pub seed: Seed,
    pub identities: Vec<Identity>,
}

/// A message ready to go out: everything read from the library, nothing sent yet.
pub struct Outgoing {
    draft: Draft,
    identity: Identity,
    forwarded: Vec<(String, String, Vec<u8>)>,
    /// IMAP / POP3: (its settings, its account in the library, the library's folder).
    server: Option<(crate::imap_sync::ImapConfig, i64, std::path::PathBuf)>,
}

/// What a library says about a message's sender: on the whitelist (people the user writes to),
/// the user's own domains, and whether pictures from the web show by themselves for them.
#[derive(Clone, Debug, Default)]
pub struct SenderTrust {
    pub trusted: bool,
    pub own_domains: Vec<String>,
    pub show_pictures: bool,
}

impl SenderTrust {
    fn read(conn: &rusqlite::Connection, from: &str) -> Result<SenderTrust> {
        let domain = from.rsplit_once('@').map(|(_, d)| format!("@{}", d.to_lowercase())).unwrap_or_default();
        let trusted: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sender_rules WHERE kind = 'allow' AND (pattern = lower(?1) OR pattern = ?2))",
            rusqlite::params![from, domain],
            |r| r.get(0),
        )?;
        let mut own_domains: Vec<String> = crate::library::own_addresses_in(conn)?.iter().filter_map(|a| a.split('@').nth(1).map(str::to_lowercase)).collect();
        own_domains.sort();
        own_domains.dedup();
        let pictures: Option<String> = rusqlite::OptionalExtension::optional(conn.query_row("SELECT value FROM settings_items WHERE key = 'pictures'", [], |r| r.get(0)))?;
        let show_pictures = !from.is_empty()
            && pictures
                .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
                .and_then(|v| v["senders"].as_array().map(|a| a.iter().any(|s| s.as_str().is_some_and(|s| s.eq_ignore_ascii_case(from)))))
                .unwrap_or(false);
        Ok(SenderTrust { trusted, own_domains, show_pictures })
    }

    /// Read from the library at `root` without opening it (a viewer of its own: read-only, never
    /// waiting long for it); nothing known when it can't be read (no one trusted, pictures ask).
    pub fn of_library(root: Option<&std::path::Path>, from: &str) -> SenderTrust {
        let Some(db) = root.map(|r| r.join("eudora.db")).filter(|p| p.is_file()) else { return SenderTrust::default() };
        let read = || -> Result<SenderTrust> {
            let conn = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
            conn.busy_timeout(std::time::Duration::from_millis(300))?;
            SenderTrust::read(&conn, from)
        };
        read().unwrap_or_default()
    }
}

/// The first From address of a message (lowercase).
pub fn sender_of(raw: &[u8]) -> String {
    crate::triage::signals(&crate::header_index::read_headers(raw)).from.to_lowercase()
}

/// Whether a message looks like a fraud, and why (None: nothing serious), with what the library
/// says about its sender. Senders on the whitelist are never judged this way.
pub fn fraud_warning_in(raw: &[u8], trust: &SenderTrust) -> Option<String> {
    let headers = crate::header_index::read_headers(raw);
    let s = crate::triage::signals(&headers);
    let subject = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("Subject")).map(|(_, v)| v.clone()).unwrap_or_default();
    // the text too (its lures, a brand it speaks for)
    let text = crate::parse::parse(raw, crate::parse::DateHints::default(), crate::library::now()).body_text;
    crate::phishing::warning(&s, &subject, &text, &trust.own_domains, trust.trusted)
}

/// A message read from a file, shown without a library (the .msg viewer): the same view as a
/// library's message, with the same safety (a fraud's links dead and no pictures). `id` is the
/// number the viewer gives it.
pub fn file_message_view(id: i64, raw: &[u8], trust: &SenderTrust) -> MessageView {
    let warning = fraud_warning_in(raw, trust);
    let d = crate::parse::display(raw, crate::library::now());
    let web_pictures = if warning.is_some() { Vec::new() } else { d.html.as_deref().map(crate::remote_images::find).unwrap_or_default() };
    let html = if warning.is_some() { d.html.as_deref().map(crate::phishing::dead_links) } else { d.html };
    let p = d.parsed;
    let inline = d.inline;
    MessageView {
        id,
        linked: Vec::new(),
        calendar: !crate::invites::calendar_texts(raw).is_empty(),
        subject: p.subject,
        from: people(&p.from),
        to: people(&p.to),
        cc: people(&p.cc),
        bcc: people(&p.bcc),
        date: p.date,
        message_id: p.message_id,
        message_id_made: p.message_id_made,
        html,
        text: d.text,
        headers: d.headers,
        attachments: p.attachments.into_iter().map(|a| AttachmentView { inline: inline.contains(&a.part), part: a.part, name: a.name, mime: a.mime, size: a.size, missing: None }).collect(),
        folders: Vec::new(),
        folder_ids: Vec::new(),
        size: p.raw_len,
        problems: p.problems.into_iter().map(|p| p.detail).collect(),
        copies: Vec::new(),
        account: None,
        web_pictures,
        warning,
    }
}

impl Library {
    /// Whether a message looks like a fraud, and why (None: nothing serious). Senders on the
    /// whitelist (people the user writes to) are never judged this way.
    pub fn fraud_warning(&self, raw: &[u8]) -> Result<Option<String>> {
        Ok(fraud_warning_in(raw, &SenderTrust::read(&self.conn, &sender_of(raw))?))
    }

    /// A message as shown.
    pub fn message_view(&self, id: i64) -> Result<MessageView> {
        let raw = self.raw_message(id)?;
        let copies = self
            .copies_of(id)?
            .into_iter()
            .map(|c| {
                let account = match (c.account, c.kind_name) {
                    (Some(a), Some(k)) => format!("{a} ({k})"),
                    (Some(a), None) => a,
                    _ => "this library".into(),
                };
                let from = match c.folder {
                    Some(f) => format!("{account} › {f}"),
                    None => account,
                };
                CopyView { id: c.id, format: c.format, size: c.size, from }
            })
            .collect();
        let warning = self.fraud_warning(&raw)?;
        let d = crate::parse::display(&raw, crate::library::now());
        // a fraud: its links dead and no pictures, with no way to turn either back on
        let web_pictures = if warning.is_some() { Vec::new() } else { d.html.as_deref().map(crate::remote_images::find).unwrap_or_default() };
        let html = if warning.is_some() { d.html.as_deref().map(crate::phishing::dead_links) } else { d.html };
        let p = d.parsed;
        let inline = d.inline;
        Ok(MessageView {
            id,
            subject: p.subject,
            from: people(&p.from),
            to: people(&p.to),
            cc: people(&p.cc),
            bcc: people(&p.bcc),
            date: p.date,
            message_id: p.message_id,
            message_id_made: p.message_id_made,
            html,
            text: d.text,
            headers: d.headers,
            attachments: {
                let mut list: Vec<AttachmentView> = p.attachments.into_iter().map(|a| AttachmentView { inline: inline.contains(&a.part), part: a.part, name: a.name, mime: a.mime, size: a.size, missing: None }).collect();
                // classic Eudora's files (kept apart from the message: found and stored, or missing)
                for (part, name, _kind, path, size) in self.old_attachments_of(id)? {
                    let mime = crate::old_attachments::mime_of(&name).to_string();
                    list.push(AttachmentView { part, name, mime, size: size.unwrap_or(0), missing: size.is_none().then_some(path), inline: false });
                }
                list
            },
            // the Online Archive is part of the mailbox: its folders shown without it, once
            folders: {
                let mut shown: Vec<String> = self.folder_paths(id)?.iter().map(|p| crate::library::shown_path(p).to_string()).filter(|p| !p.is_empty()).collect();
                shown.sort();
                shown.dedup();
                shown
            },
            folder_ids: {
                let mut stmt = self.conn.prepare_cached("SELECT folder FROM locations WHERE message = ?1 ORDER BY folder")?;
                stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?
            },
            size: p.raw_len,
            problems: p.problems.into_iter().map(|p| p.detail).collect(),
            copies,
            account: self.message_account(id)?,
            web_pictures,
            warning,
            linked: self.links_of(id)?,
            calendar: !crate::invites::calendar_texts(&raw).is_empty(),
        })
    }

    /// An attachment's bytes and type (MIME parts; uuencoded / BinHex blocks inside the text
    /// aren't offered yet).
    pub fn attachment_bytes(&self, id: i64, part: &str) -> Result<(String, Vec<u8>)> {
        if part.starts_with("eudora:") {
            // a classic Eudora file, stored in the library when found
            return self.old_attachment_bytes(id, part)?.map(|(_, mime, bytes)| (mime, bytes)).ok_or_else(|| Error::Other("that old attachment wasn't found when its message was imported".into()));
        }
        compose::part_bytes(&self.raw_message(id)?, part).ok_or_else(|| Error::Other(format!("message {id} has no attachment {part}")))
    }

    /// Every address the user can write from: their Microsoft 365 address, each shared mailbox
    /// (sent as that mailbox), IMAP / POP3 accounts with an SMTP server, and old Eudora
    /// personalities (listed, but no account sends as them yet).
    pub fn identities(&self) -> Result<Vec<Identity>> {
        let mut out = Vec::new();
        let servers = self.imap_accounts()?;
        for a in self.accounts_counted(false)? { // (no message counts: seconds on a big library)
            let Some(address) = a.address.clone() else { continue };
            match a.kind.as_str() {
                "graph" | "graph-shared" => out.push(Identity { name: a.name.clone(), address, kind: a.kind_name.into(), can_send: true }),
                // (Gmail through Google's own sign-in: sent with the Gmail API)
                "gmail" => {
                    let signed_in = crate::google::signed_in_accounts().iter().any(|g| g.eq_ignore_ascii_case(&address));
                    out.push(Identity { name: a.name.clone(), address, kind: "Gmail".into(), can_send: signed_in });
                }
                "imap" | "pop3" => {
                    let config = servers.iter().find(|c| c.address.eq_ignore_ascii_case(&address));
                    let name = config.map(|c| c.name.clone()).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| a.name.clone());
                    out.push(Identity { name, address, kind: a.kind_name.into(), can_send: config.is_some_and(|c| c.can_send()) });
                }
                "eudora" => {
                    out.push(Identity { name: a.name.clone(), address, kind: "classic Eudora".into(), can_send: false });
                    // its personalities, from where it came from ("C:\OldMail (personalities: a, b)")
                    if let Some(list) = a.source.as_deref().and_then(|s| s.split_once("(personalities: ")).map(|(_, l)| l.trim_end_matches(')')) {
                        for p in list.split(", ").filter(|p| p.contains('@')) {
                            out.push(Identity { name: a.name.clone(), address: p.to_string(), kind: "classic Eudora personality".into(), can_send: false });
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// What a new message (`mode` "new"), reply ("reply" / "replyAll"), forward ("forward") or
    /// forward of several messages, each attached ("forwardMany", `ids`) starts with.
    pub fn compose_start(&self, mode: &str, id: Option<i64>, ids: &[i64]) -> Result<ComposeStart> {
        let identities = self.identities()?;
        let mine: Vec<String> = identities.iter().map(|i| i.address.clone()).collect();
        let seed = match (mode, id) {
            ("reply", Some(id)) => compose::reply(&self.raw_message(id)?, false, &mine),
            ("replyAll", Some(id)) => compose::reply(&self.raw_message(id)?, true, &mine),
            ("forward", Some(id)) => compose::forward(id, &self.raw_message(id)?, &mine),
            ("redirect", Some(id)) => compose::redirect(id, &self.raw_message(id)?, &mine),
            ("again", Some(id)) => compose::again(id, &self.raw_message(id)?, &mine),
            ("forwardMany", _) => {
                let mut messages = Vec::new();
                for id in ids {
                    messages.push((*id, self.raw_message(*id)?));
                }
                compose::forward_many(&messages, &mine)
            }
            _ => Seed::default(),
        };
        Ok(ComposeStart { seed, identities })
    }

    /// Reads what sending `draft` needs from the library (who it's from, forwarded
    /// attachments, the account's server); `Outgoing::send` then sends it.
    pub fn prepare_send(&self, draft: Draft) -> Result<Outgoing> {
        let identity = self
            .identities()?
            .into_iter()
            .find(|i| i.address.eq_ignore_ascii_case(&draft.from))
            .ok_or_else(|| Error::Other("choose who it's from (one of your addresses)".into()))?;
        let mut forwarded = Vec::new();
        for f in &draft.forwarded {
            if let Some((mime, bytes)) = compose::part_bytes(&self.raw_message(f.message)?, &f.part) {
                forwarded.push((f.name.clone(), mime, bytes));
            }
        }
        let server = if identity.kind == "IMAP" || identity.kind == "POP3" {
            let config = self
                .imap_accounts()?
                .into_iter()
                .find(|c| c.address.eq_ignore_ascii_case(&identity.address))
                .ok_or_else(|| Error::Other(format!("{} has no settings: open Tools ▸ Personalities ▸ Change…", identity.address)))?;
            let account = self.imap_account_id(&identity.address)?.ok_or_else(|| Error::Other(format!("{} isn't an account in this library", identity.address)))?;
            Some((config, account, self.root.clone()))
        } else {
            None
        };
        Ok(Outgoing { draft, identity, forwarded, server })
    }
}

impl Outgoing {
    /// Sends it: through the account's SMTP server (IMAP / POP3; the sent copy to its Sent
    /// folder), or Microsoft 365 (the user's own address; a shared mailbox as Send As, else Send
    /// on Behalf). Returns the Message-ID.
    pub fn send(self) -> Result<String> {
        let now = chrono::Local::now();
        if let Some((config, account, root)) = self.server {
            let (mime, id) = compose::build_mime(&self.draft, &self.forwarded, now)?;
            let recipients = compose::recipients(&self.draft);
            if let crate::imap_sync::SentCopy::NotKept(why) = crate::imap_sync::send(&config, &root, account, &recipients, &mime)? {
                return Err(Error::Other(format!("Sent, but the copy for your Sent folder couldn't be kept: {why}")));
            }
            return Ok(id);
        }
        // Gmail (Google's own sign-in): the Gmail API sends it and keeps the copy in Sent Mail
        if self.identity.kind == "Gmail" {
            let (mime, id) = compose::build_mime(&self.draft, &self.forwarded, now)?;
            crate::google::GoogleClient::resume(&self.identity.address)?.send(&mime)?;
            return Ok(id);
        }
        if !self.identity.can_send {
            return Err(Error::Other(format!("no account can send as {} yet (it's an old Eudora personality)", self.identity.address)));
        }
        // the Microsoft account the address is (several may be signed in: a work account and an
        // Outlook.com one); else the default one, for a shared mailbox it opens
        let signed_in = crate::graph::GraphClient::signed_in_accounts();
        let which = signed_in.iter().find(|a| a.eq_ignore_ascii_case(&self.identity.address)).map(String::as_str);
        let mut client = crate::graph::GraphClient::resume_best(which, crate::graph::ALL_SCOPES)?; // (a personal account: its own permissions)
        let own = client.account.clone();
        let (mime, id) = compose::build_mime(&self.draft, &self.forwarded, now)?;
        if self.identity.address.eq_ignore_ascii_case(&own) {
            client.send_mime(&mime)?;
            return Ok(id);
        }
        // a shared mailbox: Send As, else Send on Behalf
        let mut shared = client.worker();
        shared.set_mailbox(Some(self.identity.address.clone()));
        match shared.send_mime(&mime) {
            Ok(()) => Ok(id),
            Err(as_error) => {
                let on_behalf = Draft { sender: Some(own), ..self.draft };
                let (mime, id) = compose::build_mime(&on_behalf, &self.forwarded, now)?;
                client.send_mime(&mime).map(|_| id).map_err(|e| Error::Other(format!("{as_error}\nAnd on behalf of it: {e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Access, Library, Silent};

    /// An .eml opened from a file (not in the library) shows, and can be answered.
    #[test]
    fn a_message_opened_from_a_file_shows_and_can_be_answered() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let raw = b"From: Ann <ann@a.com>\r\nTo: me@x.com\r\nSubject: From a file\r\nMessage-ID: <f1@a>\r\nDate: Mon, 26 Oct 2026 10:00:00 -0400\r\n\r\nHello there\r\n".to_vec();
        let id = crate::library::open_message_file(raw);
        assert!(id < 0);
        let v = library.message_view(id).unwrap();
        assert_eq!(v.subject, "From a file");
        assert!(v.folders.is_empty() && v.account.is_none());
        let start = library.compose_start("reply", Some(id), &[]).unwrap();
        assert_eq!(start.seed.subject, "Re: From a file");
        assert!(library.message_view(-999_999).is_err(), "a number never given");
    }
}
