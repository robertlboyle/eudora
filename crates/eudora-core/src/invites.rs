//! Meeting invitations.
//!
//! Microsoft 365 puts an invitation on the calendar by itself when it arrives (tentative, not
//! answered), so Eudora's copy of the calendar already holds them. `invites` lists those still
//! waiting for an answer, with what else is on the calendar then; `invitation` reads an
//! invitation email's invite file (text/calendar) and finds its event: the file's UID is the
//! event's iCalUId, or (from outside Microsoft, e.g. Google) inside it as hexadecimal. Answers go
//! through `event_respond` (the organizer is told). An invitation whose email the spam filter
//! judged spam, or that sits in a Spam / Junk folder, is left out of the list.

use mail_parser::MimeHeaders;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::calendar::Event;
use crate::error::{Error, Result};
use crate::library::{Library, now};

/// An invitation on the calendar, waiting for an answer or answered.
#[derive(Clone, Debug, Serialize)]
pub struct Invite {
    /// The event shown (a series: its next meeting).
    pub event: i64,
    /// The event an answer goes to (a series: the whole series).
    pub respond_to: i64,
    pub series: bool,
    pub calendar: i64,
    pub subject: String,
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub location: String,
    pub join_url: Option<String>,
    pub organizer: String,
    /// How many people were asked.
    pub attendees: usize,
    /// none, notResponded, accepted, tentativelyAccepted, declined.
    pub response: String,
    pub cancelled: bool,
    /// The invitation email, when it's in the library.
    pub message: Option<i64>,
    /// What else is on the calendar then (not free, not declined).
    pub conflicts: Vec<Conflict>,
    /// Why it can't be answered here (someone else's calendar; no answers asked), if so.
    pub no_answer: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Conflict {
    pub event: i64,
    pub subject: String,
    pub start: i64,
    pub end: i64,
    /// busy, tentative, oof, workingElsewhere
    pub show_as: String,
}

/// What an invitation email holds.
#[derive(Clone, Debug, Serialize)]
pub struct Invitation {
    /// REQUEST (an invitation), CANCEL, REPLY, COUNTER…
    pub method: String,
    pub uid: String,
    pub summary: String,
    /// Its event on the calendar, when there is one.
    pub invite: Option<Invite>,
    /// Answered here by email (accept, tentative, decline): for an invitation not on a calendar
    /// Eudora answers through.
    pub answered: Option<String>,
    /// Added to a calendar here (Add to my calendar, or with an answer).
    pub added: bool,
}

/// Invitations answered by email here: {message: {answer, added, at}}.
const EMAIL_ANSWERS: &str = "invite-email-answers";

/// Still waiting for the user's answer: someone else's meeting, not answered.
fn waiting(e: &Event) -> bool {
    // (an organizer who asked for no answers isn't waiting for one)
    !e.is_organizer && !e.cancelled && !e.no_answers_asked && (e.response == "notResponded" || (e.response == "none" && !e.attendees.is_empty() && !e.organizer.address.is_empty()))
}

/// A blacklist entry (an address, or @domain and its subdomains) covers this sender.
fn blocked_by(pattern: &str, address: &str) -> bool {
    match pattern.strip_prefix('@') {
        Some(domain) => {
            let theirs = address.rsplit('@').next().unwrap_or("");
            theirs == domain || theirs.ends_with(&format!(".{domain}"))
        }
        None => pattern == address,
    }
}

/// A calendar file's lines, unfolded (RFC 5545: a line going on starts with a space or a tab).
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        match (line.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(more), Some(last)) => last.push_str(more),
            _ => lines.push(line.to_string()),
        }
    }
    lines
}

/// A property's value (`UID:…`, `SUMMARY;LANGUAGE=en:…`) in the first VEVENT, else anywhere.
fn property(lines: &[String], name: &str) -> String {
    let wanted = |l: &String| {
        let (head, _) = l.split_once(':').unwrap_or((l, ""));
        head.split(';').next().unwrap_or("").eq_ignore_ascii_case(name)
    };
    let in_event = lines.iter().skip_while(|l| !l.eq_ignore_ascii_case("BEGIN:VEVENT")).find(|l| wanted(l));
    in_event
        .or_else(|| lines.iter().find(|l| wanted(l)))
        .and_then(|l| l.split_once(':').map(|(_, v)| v.replace("\\,", ",").replace("\\;", ";").replace("\\n", " ").trim().to_string()))
        .unwrap_or_default()
}

/// The invite file in a message: (METHOD, UID, SUMMARY).
pub fn calendar_in(raw: &[u8]) -> Option<(String, String, String)> {
    for text in calendar_texts(raw) {
        let lines = unfold(&text);
        let uid = property(&lines, "UID");
        if uid.is_empty() {
            continue;
        }
        let method = property(&lines, "METHOD").to_ascii_uppercase();
        return Some((if method.is_empty() { "REQUEST".into() } else { method }, uid, property(&lines, "SUMMARY")));
    }
    None
}

/// The calendar parts of an email (text/calendar, in the message itself as Outlook sends a
/// meeting, or an attached .ics), as text.
pub fn calendar_texts(raw: &[u8]) -> Vec<String> {
    let Some(message) = mail_parser::MessageParser::default().parse(raw) else { return Vec::new() };
    message
        .parts
        .iter()
        .filter(|part| {
            part.content_type().is_some_and(|c| c.ctype().eq_ignore_ascii_case("text") && c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("calendar")))
                || part.attachment_name().is_some_and(|n| n.to_ascii_lowercase().ends_with(".ics"))
        })
        .map(|part| String::from_utf8_lossy(part.contents()).to_string())
        .collect()
}

impl Library {
    /// The invitations ahead still waiting for an answer, soonest first; a series once (its
    /// next meeting). Spam left out.
    pub fn invites(&self) -> Result<Vec<Invite>> {
        let at = now();
        let rows: Vec<(i64, i64, String, Option<String>)> = {
            // (calendars Eudora can answer for: a Google invitation is answered in Google Calendar for now)
            let mut stmt = self.conn.prepare("SELECT e.id, e.calendar, e.event, e.series FROM events e JOIN calendars c ON c.id = e.calendar WHERE e.end > ?1 AND e.kind <> 'seriesMaster' AND c.can_edit ORDER BY e.start")?;
            stmt.query_map(params![at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        let mut series_seen = std::collections::HashSet::new();
        for (id, calendar, json, series) in rows {
            let e: Event = serde_json::from_str(&json).unwrap_or_default();
            if !waiting(&e) {
                continue;
            }
            if let Some(s) = &series {
                if !series_seen.insert((calendar, s.clone())) {
                    continue;
                }
            }
            let invite = self.invite_of(id, calendar, &e, series.as_deref())?;
            if invite.message.is_some_and(|m| self.invite_is_spam(m).unwrap_or(false)) {
                continue;
            }
            out.push(invite);
        }
        Ok(out)
    }

    /// How many invitations wait for an answer (the Invites button).
    pub fn invites_waiting(&self) -> Result<usize> {
        Ok(self.invites()?.len())
    }

    /// An email's invitation and its event on the calendar.
    pub fn invitation(&self, message: i64) -> Result<Option<Invitation>> {
        let raw = self.raw_message(message)?;
        let Some((method, uid, summary)) = calendar_in(&raw) else { return Ok(None) };
        // Microsoft's own id, or the file's UID inside Microsoft's as hexadecimal
        let hex: String = uid.bytes().map(|b| format!("{b:02X}")).collect();
        let found: Option<(i64, i64, String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT id, calendar, event, series FROM events
                 WHERE (json_extract(event, '$.ical_uid') = ?1 COLLATE NOCASE OR instr(upper(json_extract(event, '$.ical_uid')), ?2) > 0)
                 ORDER BY kind = 'seriesMaster', end < ?3, start LIMIT 1",
                params![uid, hex, now()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let invite = match found {
            Some((id, calendar, json, series)) => {
                let e: Event = serde_json::from_str(&json).unwrap_or_default();
                let mut invite = self.invite_of(id, calendar, &e, series.as_deref())?;
                invite.message = Some(message);
                Some(invite)
            }
            None => None,
        };
        let (answered, added) = self.email_answer_of(message)?;
        Ok(Some(Invitation { method, uid, summary, invite, answered, added }))
    }

    /// Adds an invitation email's meeting to a calendar (the one given, else the default one of
    /// the account it came to, else any default one Eudora can change), for invitations not on a
    /// calendar here (an IMAP account's, one not read from the server yet). Never with its people:
    /// nobody is sent anything. Returns the new event's id.
    pub fn invitation_add(&self, message: i64, calendar: Option<i64>) -> Result<i64> {
        let raw = self.raw_message(message)?;
        let event = calendar_texts(&raw)
            .iter()
            .find_map(|t| crate::ics::read(t).events.into_iter().next())
            .ok_or_else(|| Error::Other("this email has no meeting Eudora can read".into()))?;
        let account = self.message_account(message)?;
        let calendar = match calendar {
            Some(c) => c,
            None => self
                .conn
                .query_row(
                    "SELECT id FROM calendars WHERE can_edit ORDER BY account = ?1 DESC, is_default DESC, id LIMIT 1",
                    params![account.unwrap_or(-1)],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Error::Other("there's no calendar to add it to: add a Microsoft 365 or Google account first".into()))?,
        };
        let id = self.new_appointment(&crate::meetings::NewEvent::from_ics(calendar, &event))?;
        self.note_email_answer(message, None, true)?;
        Ok(id)
    }

    /// Answers an invitation by email, for one not on a calendar Eudora answers through (an IMAP
    /// account's; one not read from the server yet): an iCalendar REPLY to the organizer, from
    /// whichever of the user's addresses was invited, put in the Out mailbox to go at once;
    /// accepted or tentative, the meeting is added to the user's calendar too (once). Returns
    /// what was done, in words.
    pub fn invitation_answer(&self, message: i64, answer: &str, comment: Option<&str>) -> Result<String> {
        if !["accept", "tentative", "decline"].contains(&answer) {
            return Err(Error::Other(format!("“{answer}” isn't an answer: accept, tentative or decline")));
        }
        let raw = self.raw_message(message)?;
        let request = calendar_texts(&raw)
            .into_iter()
            .find(|t| {
                let f = crate::ics::read(t);
                f.method.as_deref().is_none_or(|m| m == "REQUEST") && !f.events.is_empty()
            })
            .ok_or_else(|| Error::Other("this email has no invitation Eudora can answer".into()))?;
        let (organizer, attendees) = crate::ics::organizer_and_attendees(&request);
        let parsed = crate::parse::display(&raw, now()).parsed;
        let organizer = organizer
            .or_else(|| parsed.from.first().map(|a| a.address.to_lowercase()))
            .ok_or_else(|| Error::Other("the invitation doesn't say who to answer".into()))?;
        // the user's address that was invited (else the one the email came to), able to send
        let identities: Vec<crate::outgoing::Identity> = self.identities()?.into_iter().filter(|i| i.can_send).collect();
        let account_address: Option<String> = match self.message_account(message)? {
            Some(a) => self.conn.query_row("SELECT lower(address) FROM accounts WHERE id = ?1", params![a], |r| r.get(0)).optional()?.flatten(),
            None => None,
        };
        let me = identities
            .iter()
            .find(|i| attendees.contains(&i.address.to_lowercase()))
            .or_else(|| identities.iter().find(|i| attendees.is_empty() && account_address.as_deref() == Some(i.address.to_lowercase().as_str())))
            .ok_or_else(|| Error::Other("none of the addresses you can send from was invited".into()))?;
        let summary = crate::ics::read(&request).events.first().map(|e| e.summary.clone()).unwrap_or_default();
        let ics = crate::ics::reply(&request, &me.address, &me.name, answer, comment, now())
            .ok_or_else(|| Error::Other("the invitation couldn't be read to answer it".into()))?;
        let (word, sentence) = match answer {
            "accept" => ("Accepted", "has accepted"),
            "tentative" => ("Tentative", "has tentatively accepted"),
            _ => ("Declined", "has declined"),
        };
        let who = if me.name.trim().is_empty() { me.address.clone() } else { me.name.clone() };
        let mut text = format!("{who} {sentence} this invitation.");
        if let Some(c) = comment.map(str::trim).filter(|c| !c.is_empty()) {
            text.push_str("\n\n");
            text.push_str(c);
        }
        let draft = crate::compose::Draft {
            from: me.address.clone(),
            from_name: me.name.clone(),
            to: organizer.clone(),
            subject: format!("{word}: {summary}"),
            text,
            in_reply_to: (!parsed.message_id_made && !parsed.message_id.is_empty()).then(|| parsed.message_id.clone()),
            calendar: Some(ics),
            ..Default::default()
        };
        self.queue_message(&draft, now())?;
        let (_, added_before) = self.email_answer_of(message)?;
        let mut said = format!("{word}: the answer goes to {organizer}");
        let mut added = added_before;
        if answer != "decline" && !added_before {
            match self.invitation_add(message, None) {
                Ok(_) => {
                    added = true;
                    said.push_str(", and the meeting is on your calendar");
                }
                Err(e) => said.push_str(&format!(" (not added to a calendar: {e})")),
            }
        }
        self.note_email_answer(message, Some(answer), added)?;
        Ok(said)
    }

    /// How an invitation email was answered here, and whether its meeting was added.
    fn email_answer_of(&self, message: i64) -> Result<(Option<String>, bool)> {
        let map: serde_json::Value = self.setting(EMAIL_ANSWERS)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let entry = &map[message.to_string()];
        Ok((entry["answer"].as_str().map(str::to_string), entry["added"].as_bool().unwrap_or(false)))
    }

    fn note_email_answer(&self, message: i64, answer: Option<&str>, added: bool) -> Result<()> {
        let mut map: serde_json::Value = self.setting(EMAIL_ANSWERS)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_else(|| serde_json::json!({}));
        let key = message.to_string();
        let mut entry = map.get(&key).cloned().unwrap_or_else(|| serde_json::json!({}));
        if let Some(a) = answer {
            entry["answer"] = a.into();
        }
        entry["added"] = (entry["added"].as_bool().unwrap_or(false) || added).into();
        entry["at"] = now().into();
        map[key] = entry;
        self.set_setting(EMAIL_ANSWERS, &map.to_string())
    }

    /// An event as an invitation: who asked, what clashes, its email.
    fn invite_of(&self, id: i64, calendar: i64, e: &Event, series: Option<&str>) -> Result<Invite> {
        // a series is answered as a whole (its master)
        let master: Option<i64> = match series {
            Some(s) => self.conn.query_row("SELECT id FROM events WHERE calendar = ?1 AND server_id = ?2", params![calendar, s], |r| r.get(0)).optional()?,
            None => None,
        };
        let conflicts = if e.all_day {
            Vec::new()
        } else {
            self.events_between(e.start, e.end)?
                .into_iter()
                .filter(|o| o.id != id && !(o.subject == e.subject && o.start == e.start && o.end == e.end) && !o.all_day && !o.cancelled && o.show_as != "free" && o.response != "declined" && o.start < e.end && o.end > e.start)
                .map(|o| Conflict { event: o.id, subject: o.subject, start: o.start, end: o.end, show_as: o.show_as })
                .collect()
        };
        let organizer = match (e.organizer.name.as_str(), e.organizer.address.as_str()) {
            ("", a) => a.to_string(),
            (n, "") => n.to_string(),
            (n, a) if n.eq_ignore_ascii_case(a) => a.to_string(),
            (n, a) => format!("{n} <{a}>"),
        };
        Ok(Invite {
            event: id,
            respond_to: master.unwrap_or(id),
            series: series.is_some(),
            calendar,
            subject: e.subject.clone(),
            start: e.start,
            end: e.end,
            all_day: e.all_day,
            location: e.location.clone(),
            join_url: e.join_url.clone(),
            organizer,
            attendees: e.attendees.len(),
            response: e.response.clone(),
            cancelled: e.cancelled,
            message: self.invite_message(&e.organizer.address, &e.subject)?,
            conflicts,
            no_answer: self.why_no_answer(calendar, e)?,
        })
    }

    /// The invitation email: the newest from the organizer naming the meeting.
    fn invite_message(&self, organizer: &str, subject: &str) -> Result<Option<i64>> {
        if organizer.is_empty() || subject.is_empty() {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT m.id FROM addresses a JOIN message_addresses ma ON ma.address = a.id AND ma.role = 0 JOIN messages m ON m.id = ma.message
                 WHERE a.address = lower(?1) AND instr(m.subject, ?2) > 0 ORDER BY m.date DESC LIMIT 1",
                params![organizer, subject],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// An invitation reported as spam: its email marked spam as Message ▸ Spam does (the sender
    /// blocked in that mailbox, the email to Spam), else the organizer blocked; and the meeting
    /// taken off the calendar without a word to the sender (a decline would tell them the address
    /// works). Returns what was done, in words.
    pub fn invite_spam(&self, event: i64, series: bool) -> Result<Vec<String>> {
        let (_, _, e) = self.event_for_change(event)?;
        let message = self.invite_message(&e.organizer.address, &e.subject)?;
        self.remove_invite_quietly(event, series)?;
        let mut said = vec![format!("“{}” taken off your calendar (the sender isn't told)", e.subject)];
        let folder: Option<i64> = match message {
            Some(m) => self.conn.query_row("SELECT folder FROM locations WHERE message = ?1 LIMIT 1", params![m], |r| r.get(0)).optional()?,
            None => None,
        };
        match (message, folder) {
            (Some(m), Some(folder)) => said.extend(self.mark_and_move(&[(m, folder)], true)?.0),
            _ if !e.organizer.address.is_empty() => {
                self.set_sender_rule(&e.organizer.address, "block", None, "invitation reported as spam")?;
                said.push(format!("{} blocked", e.organizer.address));
            }
            _ => {}
        }
        Ok(said)
    }

    /// Takes an invitation off the calendar without answering it (a series: all of it).
    fn remove_invite_quietly(&self, id: i64, series: bool) -> Result<()> {
        let (account, server_id, e) = self.event_for_change(id)?;
        let calendar: i64 = self.conn.query_row("SELECT calendar FROM events WHERE id = ?1", params![id], |r| r.get(0))?;
        let target = if series { e.series.clone().unwrap_or_else(|| server_id.clone()) } else { server_id.clone() };
        self.in_transaction(|| {
            // (a plain delete: for someone else's meeting it tells no one)
            self.queue_event_change(serde_json::json!({ "event": id, "account": account, "server_id": target, "action": "delete" }))?;
            self.conn.execute("DELETE FROM events WHERE id = ?1", params![id])?;
            if series {
                self.conn.execute("DELETE FROM events WHERE calendar = ?1 AND (series = ?2 OR server_id = ?2)", params![calendar, target])?;
            }
            Ok(())
        })
    }

    /// Invitations from blocked senders leave the calendar quietly (Microsoft 365 puts each
    /// arriving invitation there before Eudora's filter sorts its email). Returns how many.
    pub fn remove_blocked_invites(&self) -> Result<usize> {
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT e.id, e.event, COALESCE(a.address, '') FROM events e JOIN calendars c ON c.id = e.calendar JOIN accounts a ON a.id = c.account
                 WHERE e.end > ?1 AND e.kind <> 'seriesMaster'",
            )?;
            stmt.query_map(params![now()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let blocks: Vec<(String, String)> = {
            let mut stmt = self.conn.prepare("SELECT pattern, mailbox FROM sender_rules WHERE kind = 'block'")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?.to_lowercase(), r.get::<_, String>(1)?.to_lowercase())))?.collect::<rusqlite::Result<_>>()?
        };
        if blocks.is_empty() {
            return Ok(0);
        }
        let mut removed = 0;
        let mut series_done = std::collections::HashSet::new();
        for (id, json, mailbox) in rows {
            let e: Event = serde_json::from_str(&json).unwrap_or_default();
            let from = e.organizer.address.to_lowercase();
            if !waiting(&e) || from.is_empty() || !blocks.iter().any(|(p, m)| (m.is_empty() || *m == mailbox.to_lowercase()) && blocked_by(p, &from)) {
                continue;
            }
            if let Some(s) = &e.series {
                if !series_done.insert(s.clone()) {
                    continue;
                }
            }
            self.remove_invite_quietly(id, e.series.is_some())?;
            removed += 1;
        }
        Ok(removed)
    }

    /// The spam filter judged the email spam, or it's in a Spam / Junk folder.
    fn invite_is_spam(&self, message: i64) -> Result<bool> {
        let judged: bool = self.conn.query_row("SELECT EXISTS (SELECT 1 FROM triage WHERE message = ?1 AND category IN ('spam', 'blocked'))", params![message], |r| r.get(0))?;
        if judged {
            return Ok(true);
        }
        let mut stmt = self.conn.prepare_cached("SELECT f.path FROM locations l JOIN folders f ON f.id = l.folder WHERE l.message = ?1")?;
        let paths: Vec<String> = stmt.query_map(params![message], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(paths.iter().any(|p| {
            let top = p.strip_prefix("Online Archive/").unwrap_or(p).split('/').next().unwrap_or("").to_lowercase();
            matches!(top.as_str(), "spam" | "junk" | "junk email" | "junk e-mail")
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outlook sends a meeting inside the message (multipart/alternative), not as an attachment.
    #[test]
    fn a_meeting_inside_the_message_is_found() {
        let mail = "From: ann@x.com\r\nTo: me@y.com\r\nSubject: Budget review\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nWhen: Thursday\r\n--b\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\n\
BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:040000008200E00074C5B7101A82E008\r\nSUMMARY:Budget review\r\nDTSTART:20261105T190000Z\r\nDTEND:20261105T200000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n";
        assert_eq!(calendar_texts(mail.as_bytes()).len(), 1);
        let (method, _, summary) = calendar_in(mail.as_bytes()).unwrap();
        assert_eq!((method.as_str(), summary.as_str()), ("REQUEST", "Budget review"));
        let e = crate::ics::read(&calendar_texts(mail.as_bytes())[0]).events.remove(0);
        let new = crate::meetings::NewEvent::from_ics(7, &e);
        assert_eq!((new.subject.as_str(), new.attendees.as_str(), new.calendar), ("Budget review", "", 7), "added without its people");
        assert!(calendar_texts(b"From: a@b.c\r\nSubject: hi\r\n\r\nno meeting\r\n").is_empty());
    }

    #[test]
    fn an_invite_file_is_read_and_its_lines_unfolded() {
        let mail = "From: joel@x.com\r\nTo: me@y.com\r\nSubject: Invitation\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nSee you\r\n--b\r\nContent-Type: text/calendar; charset=UTF-8; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:7abc1defg2hij3klm4n\r\n o5pqrst@google.com\r\nSUMMARY;LANGUAGE=en:Acme\\, and Example Co\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n";
        let (method, uid, summary) = calendar_in(mail.as_bytes()).unwrap();
        assert_eq!((method.as_str(), uid.as_str(), summary.as_str()), ("REQUEST", "7abc1defg2hij3klm4no5pqrst@google.com", "Acme, and Example Co"));
        assert!(calendar_in(b"From: a@b.c\r\nSubject: hi\r\n\r\nno invite\r\n").is_none());
    }

    #[test]
    fn a_block_covers_the_address_or_its_domain() {
        assert!(blocked_by("spam@x.com", "spam@x.com"));
        assert!(!blocked_by("spam@x.com", "ann@x.com"));
        assert!(blocked_by("@x.com", "ann@mail.x.com"));
        assert!(!blocked_by("@x.com", "ann@notx.com"));
    }

    #[test]
    fn only_others_meetings_not_answered_are_waiting() {
        let mut e = Event { response: "notResponded".into(), ..Default::default() };
        assert!(waiting(&e));
        e.response = "accepted".into();
        assert!(!waiting(&e));
        // a holiday calendar's day: no one asked
        e.response = "none".into();
        assert!(!waiting(&e));
        e.is_organizer = true;
        e.response = "notResponded".into();
        assert!(!waiting(&e), "your own meeting");
    }
}
