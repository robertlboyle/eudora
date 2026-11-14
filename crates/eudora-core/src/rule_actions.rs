//! What a rule does besides filing (classic Eudora's filter actions): label, priority, status,
//! junk, change the subject, forward / redirect / reply (sent through the Out mailbox, at once),
//! and notify (the window shows it).
//!
//! Mail a rule sends by itself is guarded, so two automatic senders can't answer each other for
//! ever and mailing lists never hear from Eudora: nothing goes out for mail marked automatic
//! (Auto-Submitted), bulk or list mail (Precedence, List-Id), mail from the user's own
//! addresses, or mail with no sender; and a rule's reply goes to the same address at most once
//! a day.

use rusqlite::params;

use crate::compose::{Draft, Origin};
use crate::library::{Library, now};
use crate::marks::Status;
use crate::rules::{Action, Facts};

/// Setting: when each address last got a rule's reply (address → unix seconds).
const REPLIED_KEY: &str = "rule-replies";

impl Library {
    /// Does one action of a rule on a message (in folder `folder`). Err: why it couldn't (for
    /// the run's report; nothing is dropped silently).
    pub(crate) fn rule_action(&self, id: i64, folder: i64, action: &Action, rule: &str, facts: &Facts) -> std::result::Result<(), String> {
        let e = |e: crate::Error| e.to_string();
        match action {
            Action::Label { label } => {
                let name = self.label_by_classic_name(label).map_err(e)?;
                self.set_label(&[id], &name, true).map_err(e)?;
            }
            Action::Priority { level: -1 } => {
                self.shift_priority(&[id], -1).map_err(e)?;
            }
            Action::Priority { level: -2 } => {
                self.shift_priority(&[id], 1).map_err(e)?;
            }
            Action::Priority { level } => {
                let level = u8::try_from(*level).ok().filter(|l| (1..=5).contains(l)).ok_or("a priority is 1 to 5")?;
                self.set_priority(&[id], Some(level)).map_err(e)?;
            }
            Action::Status { status } => {
                let status = Status::parse(status).ok_or_else(|| format!("there's no status \"{status}\""))?;
                self.set_status(&[id], status).map_err(e)?;
            }
            Action::Junk => {
                self.mark_and_move(&[(id, folder)], true).map_err(e)?;
            }
            Action::Subject { text } => {
                let old: String = self.conn.query_row("SELECT subject FROM messages WHERE id = ?1", params![id], |r| r.get(0)).map_err(|x| x.to_string())?;
                // "^" stands for the subject it had (classic Eudora's)
                let new = if text.contains('^') { text.replace('^', &old) } else { text.clone() };
                self.conn.execute("UPDATE messages SET subject = ?2 WHERE id = ?1", params![id, new]).map_err(|x| x.to_string())?;
            }
            Action::Forward { to } | Action::Redirect { to } => {
                self.may_send_automatically(facts)?;
                let raw = self.raw_message(id).map_err(e)?;
                let mine = self.own_addresses().map_err(e)?;
                let (seed, kind) = match action {
                    Action::Forward { .. } => (crate::compose::forward(id, &raw, &mine), "forward"),
                    _ => (crate::compose::redirect(id, &raw, &mine), "redirect"),
                };
                let mut draft = self.automatic_draft(folder, &seed)?;
                draft.to = to.clone();
                draft.origin = Some(Origin { message: id, kind: kind.into() });
                self.queue_message(&draft, now()).map_err(e)?;
            }
            Action::Reply { with } => {
                self.may_send_automatically(facts)?;
                let raw = self.raw_message(id).map_err(e)?;
                let mine = self.own_addresses().map_err(e)?;
                let seed = crate::compose::reply(&raw, false, &mine);
                let to = seed.to.to_lowercase();
                if self.replied_recently(&to).map_err(e)? {
                    return Err("already answered this sender today".into());
                }
                let mut draft = self.automatic_draft(folder, &seed)?;
                draft.html = format!("{}{}", reply_text(with)?, seed.html);
                draft.origin = Some(Origin { message: id, kind: "reply".into() });
                self.queue_message(&draft, now()).map_err(e)?;
                self.note_replied(&to).map_err(e)?;
            }
            Action::Plugin { plugin, command } => {
                let out = self.plugin_run(plugin, command, &serde_json::json!({ "message": id, "folder": folder, "rule": rule })).map_err(e)?;
                if let Some(problem) = out.error {
                    return Err(format!("the plugin: {problem}"));
                }
            }
            Action::Notify => {
                let subject = facts.headers.iter().find(|(n, _)| n == "subject").map(|(_, v)| v.as_str()).unwrap_or("");
                let from = facts.headers.iter().find(|(n, _)| n == "from").map(|(_, v)| v.as_str()).unwrap_or("");
                notify(&self.root, id, rule, from, subject).map_err(|x| x.to_string())?;
            }
            // spoken or played by the window (a rule may run in the background download)
            Action::Speak { who, subject } => {
                let header = |name: &str| facts.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()).unwrap_or_default();
                let mut said = Vec::new();
                if *who {
                    // the sender's name, else their address ("From: Ann Smith.")
                    let from = crate::parse::parse(format!("From: {}

", header("from")).as_bytes(), Default::default(), 0).from;
                    let name = from.first().map(|a| if a.name.trim().is_empty() { a.address.clone() } else { a.name.clone() }).unwrap_or_else(|| header("from"));
                    said.push(format!("From: {name}."));
                }
                if *subject {
                    // (Re: and Fwd: left out, as classic did)
                    let s = header("subject");
                    let mut s = s.trim();
                    loop {
                        let lower = s.to_ascii_lowercase();
                        match ["re:", "fw:", "fwd:"].iter().find(|p| lower.starts_with(**p)) {
                            Some(p) => s = s[p.len()..].trim_start(),
                            None => break,
                        }
                    }
                    said.push(if s.is_empty() { "No subject.".to_string() } else { s.to_string() });
                }
                if !said.is_empty() {
                    notice(&self.root, &serde_json::json!({ "at": now(), "message": id, "rule": rule, "speak": said.join(" ") })).map_err(|x| x.to_string())?;
                }
            }
            Action::Sound { file } => {
                notice(&self.root, &serde_json::json!({ "at": now(), "message": id, "rule": rule, "sound": file })).map_err(|x| x.to_string())?;
            }
            Action::Transfer { .. } | Action::Copy { .. } | Action::Delete | Action::Sort | Action::Stop => {}
        }
        Ok(())
    }

    /// A label by its name, or by its number in the list (classic filters say "label 3").
    fn label_by_classic_name(&self, label: &str) -> crate::Result<String> {
        if let Ok(n) = label.trim().parse::<usize>() {
            if let Some(l) = self.label_list()?.get(n.wrapping_sub(1)) {
                return Ok(l.name.clone());
            }
        }
        Ok(label.trim().to_string())
    }

    /// Err when mail answering this one mustn't go out by itself (see the module's note).
    fn may_send_automatically(&self, facts: &Facts) -> std::result::Result<(), String> {
        let header = |name: &str| facts.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.trim().to_ascii_lowercase());
        if header("auto-submitted").is_some_and(|v| v != "no") {
            return Err("not sent: the message was sent automatically".into());
        }
        if header("precedence").is_some_and(|v| matches!(v.as_str(), "bulk" | "list" | "junk")) || header("list-id").is_some() {
            return Err("not sent: bulk or mailing-list mail".into());
        }
        let from = header("from").unwrap_or_default();
        if !from.contains('@') {
            return Err("not sent: the message has no sender".into());
        }
        let mine = self.own_addresses().map_err(|e| e.to_string())?;
        if mine.iter().any(|m| from.contains(&m.to_ascii_lowercase())) {
            return Err("not sent: the message is from you".into());
        }
        Ok(())
    }

    /// A message sent by a rule: from the account the mail came to when it can send, else the
    /// address the mail was sent to, else the first account that can send.
    fn automatic_draft(&self, folder: i64, seed: &crate::compose::Seed) -> std::result::Result<Draft, String> {
        let identities = self.identities().map_err(|e| e.to_string())?;
        let account: Option<String> = self
            .conn
            .query_row("SELECT a.address FROM folders f JOIN accounts a ON a.id = f.account WHERE f.id = ?1", params![folder], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let usable = |a: &str| identities.iter().find(|i| i.can_send && i.address.eq_ignore_ascii_case(a));
        let from = account
            .as_deref()
            .and_then(usable)
            .or_else(|| seed.to_me.as_deref().and_then(usable))
            .or_else(|| identities.iter().find(|i| i.can_send))
            .ok_or("not sent: no account can send yet")?;
        Ok(Draft {
            from: from.address.clone(),
            from_name: from.name.clone(),
            to: seed.to.clone(),
            cc: seed.cc.clone(),
            subject: seed.subject.clone(),
            html: seed.html.clone(),
            in_reply_to: seed.in_reply_to.clone(),
            references: seed.references.clone(),
            forwarded: seed.forwarded.clone(),
            redirect_of: seed.redirect_of.clone(),
            ..Default::default()
        })
    }

    fn replied_recently(&self, to: &str) -> crate::Result<bool> {
        let map: std::collections::BTreeMap<String, i64> = self.setting(REPLIED_KEY)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        Ok(map.get(to).is_some_and(|at| now() - at < 86_400))
    }

    fn note_replied(&self, to: &str) -> crate::Result<()> {
        let mut map: std::collections::BTreeMap<String, i64> = self.setting(REPLIED_KEY)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let t = now();
        map.retain(|_, at| t - *at < 7 * 86_400);
        map.insert(to.to_string(), t);
        self.set_setting(REPLIED_KEY, &serde_json::to_string(&map)?)
    }
}

/// The text a rule replies with: classic Eudora's stationery file (its text after the
/// headers), or the words written in the rule.
fn reply_text(with: &str) -> std::result::Result<String, String> {
    let with = with.trim();
    let looks_like_file = with.ends_with(".sta") || with.contains('\\') || with.contains('/');
    let text = if looks_like_file {
        let bytes = std::fs::read(with).map_err(|_| format!("the stationery file {with} isn't there"))?;
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        // a stationery file is a message: headers, a blank line, then its text
        match text.split_once("\n\n") {
            Some((head, body)) if head.lines().all(|l| l.contains(':') || l.starts_with([' ', '\t'])) => body.to_string(),
            _ => text,
        }
    } else {
        with.to_string()
    };
    if text.trim().is_empty() {
        return Err("the reply has no text".into());
    }
    let escaped = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    Ok(format!("<p>{}</p>", escaped.trim().replace('\n', "<br>")))
}

/// A notice for the window (`<library>/status/notify.jsonl`, one line each; the window shows
/// and clears them).
pub fn notify(root: &std::path::Path, id: i64, rule: &str, from: &str, subject: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = root.join("status");
    std::fs::create_dir_all(&dir)?;
    let line = serde_json::json!({ "at": now(), "message": id, "rule": rule, "from": from, "subject": subject });
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("notify.jsonl"))?;
    writeln!(file, "{line}")
}

/// Something for the window to say or play (a rule's Speak / Play Sound), in the same file as
/// the notices.
pub fn notice(root: &std::path::Path, line: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    let dir = root.join("status");
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("notify.jsonl"))?;
    writeln!(file, "{line}")
}

/// The notices waiting for the window, taken (the file is emptied).
pub fn take_notices(root: &std::path::Path) -> Vec<serde_json::Value> {
    let path = root.join("status").join("notify.jsonl");
    let taken = root.join("status").join("notify.taking");
    if std::fs::rename(&path, &taken).is_err() {
        return Vec::new();
    }
    let text = std::fs::read_to_string(&taken).unwrap_or_default();
    let _ = std::fs::remove_file(&taken);
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_the_rule_s_words_or_its_stationery() {
        assert_eq!(reply_text("Thanks, got it.\nPat").unwrap(), "<p>Thanks, got it.<br>Pat</p>");
        assert!(reply_text(r"C:\Eudora\Stationery\gone.sta").unwrap_err().contains("isn't there"));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("thanks.sta");
        std::fs::write(&file, "To: \r\nSubject: Thanks\r\n\r\nThank you <3\r\n").unwrap();
        assert_eq!(reply_text(file.to_str().unwrap()).unwrap(), "<p>Thank you &lt;3</p>");
    }

    #[test]
    fn notices_are_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        notify(dir.path(), 7, "Boss", "Ann <ann@x.com>", "Now").unwrap();
        notify(dir.path(), 8, "Boss", "Ann <ann@x.com>", "Again").unwrap();
        let taken = take_notices(dir.path());
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[1]["subject"], "Again");
        assert!(take_notices(dir.path()).is_empty());
    }
}
