//! The Out mailbox: messages queued to be sent later (Send Later: a date and time) or sent as a
//! batch (File ▸ Send Queued Messages), as in classic Eudora. A queued message waits in
//! `pending_ops` (kind "outbox", the draft and when to send); whatever Eudora program runs at
//! that time sends it: the window, or the download (`download.cmd`) when the window is closed.
//! Missed while both were closed: sent at the next start.
//!
//! A message is **claimed** before it's sent (kind "outbox" → "outbox-sending" in one update:
//! only one program wins), so it never goes out twice. One whose sending was cut short (the
//! program stopped mid-send) stays claimed and says so: it may or may not have gone, so it's
//! never sent again by itself. A send that fails is tried again later (after 1, 2, 4 … 60 min).

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::compose::Draft;
use crate::error::{Error, Result};
use crate::library::{Library, now};

/// A queued message, for the Out mailbox.
#[derive(Clone, Debug, Serialize)]
pub struct Queued {
    pub id: i64,
    /// When it's to be sent (unix seconds); 0 = with the next Send Queued Messages.
    pub send_at: i64,
    pub to: String,
    pub subject: String,
    pub from: String,
    /// "queued", "sending" (claimed: being sent, or cut short), "failed" (tried; waits for the next try)
    pub state: String,
    pub tries: i64,
    pub last_error: Option<String>,
}

/// What sending the due messages did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sent {
    pub sent: u64,
    pub failed: u64,
    /// For the log: "To ann@x.com: Lunch" or why not.
    pub notes: Vec<String>,
}

impl Library {
    /// Queues a message to be sent at `send_at` (unix seconds; 0 = with the next Send Queued
    /// Messages). Returns its id in the Out mailbox.
    pub fn queue_message(&self, draft: &Draft, send_at: i64) -> Result<i64> {
        let payload = serde_json::json!({ "draft": draft, "send_at": send_at, "queued": now() });
        self.conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'outbox', ?2)", params![now(), payload.to_string()])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The Out mailbox, soonest first.
    pub fn outbox(&self) -> Result<Vec<Queued>> {
        let mut stmt = self.conn.prepare("SELECT id, kind, payload, attempts, last_error FROM pending_ops WHERE kind IN ('outbox', 'outbox-sending')")?;
        let rows: Vec<(i64, String, String, i64, Option<String>)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut out: Vec<Queued> = rows
            .into_iter()
            .filter_map(|(id, kind, payload, tries, last_error)| {
                let v: serde_json::Value = serde_json::from_str(&payload).ok()?;
                let draft: Draft = serde_json::from_value(v["draft"].clone()).ok()?;
                let state = if kind == "outbox-sending" { "sending" } else if tries > 0 { "failed" } else { "queued" };
                Some(Queued { id, send_at: v["send_at"].as_i64().unwrap_or(0), to: draft.to, subject: draft.subject, from: draft.from, state: state.into(), tries, last_error })
            })
            .collect();
        out.sort_by_key(|q| (q.send_at, q.id));
        Ok(out)
    }

    /// A queued message's draft (to change it: unqueued, it opens as a draft again).
    pub fn unqueue(&self, id: i64) -> Result<Draft> {
        let payload: String = self
            .conn
            .query_row("SELECT payload FROM pending_ops WHERE id = ?1 AND kind IN ('outbox', 'outbox-sending')", params![id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::Other("that message isn't waiting in the Out mailbox any more".into()))?;
        let v: serde_json::Value = serde_json::from_str(&payload)?;
        let draft: Draft = serde_json::from_value(v["draft"].clone())?;
        self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![id])?;
        Ok(draft)
    }

    /// Changes when a queued message goes (0 = with the next Send Queued Messages); also
    /// releases one whose sending was cut short, when the user says to send it again.
    pub fn reschedule(&self, id: i64, send_at: i64) -> Result<()> {
        let payload: String = self.conn.query_row("SELECT payload FROM pending_ops WHERE id = ?1 AND kind IN ('outbox', 'outbox-sending')", params![id], |r| r.get(0))?;
        let mut v: serde_json::Value = serde_json::from_str(&payload)?;
        v["send_at"] = send_at.into();
        v.as_object_mut().map(|o| o.remove("retry_at"));
        self.conn.execute("UPDATE pending_ops SET kind = 'outbox', payload = ?2, attempts = 0, last_error = NULL WHERE id = ?1", params![id, v.to_string()])?;
        Ok(())
    }

    /// Claims the messages due now (`all`: every queued one, Send Queued Messages): each moves
    /// to "sending" in one update, so another program can't take it too. Returns (id, draft).
    pub fn claim_due(&self, all: bool) -> Result<Vec<(i64, Draft)>> {
        let t = now();
        let mut stmt = self.conn.prepare("SELECT id, payload FROM pending_ops WHERE kind = 'outbox'")?;
        let rows: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut claimed = Vec::new();
        for (id, payload) in rows {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) else { continue };
            let send_at = v["send_at"].as_i64().unwrap_or(0);
            let retry_at = v["retry_at"].as_i64().unwrap_or(0);
            let due = if all { retry_at <= t } else { send_at > 0 && send_at <= t && retry_at <= t };
            if !due {
                continue;
            }
            let Ok(draft) = serde_json::from_value::<Draft>(v["draft"].clone()) else { continue };
            if self.conn.execute("UPDATE pending_ops SET kind = 'outbox-sending' WHERE id = ?1 AND kind = 'outbox'", params![id])? == 1 {
                claimed.push((id, draft));
            }
        }
        Ok(claimed)
    }

    /// A claimed message went: out of the Out mailbox.
    pub fn sent_queued(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// A claimed message didn't go: back in the queue, tried again after a while (1, 2, 4 … 60
    /// minutes).
    pub fn send_failed(&self, id: i64, why: &str) -> Result<()> {
        let (payload, tries): (String, i64) = self.conn.query_row("SELECT payload, attempts FROM pending_ops WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut v: serde_json::Value = serde_json::from_str(&payload)?;
        let wait = 60 * (1i64 << tries.min(6)).min(60);
        v["retry_at"] = (now() + wait).into();
        self.conn.execute(
            "UPDATE pending_ops SET kind = 'outbox', payload = ?2, attempts = attempts + 1, last_error = ?3 WHERE id = ?1",
            params![id, v.to_string(), why],
        )?;
        Ok(())
    }
}

/// Sends what's due (`all`: everything queued) from `library`: claims, then sends each through
/// its account (the library is only touched before and after each send). For the window's
/// timer, the download's watch, and Send Queued Messages.
pub fn send_due(library: &Library, all: bool) -> Result<Sent> {
    let mut done = Sent::default();
    if crate::graph_sync::is_offline(&library.root) {
        return Ok(done); // Work Offline: it waits in Out
    }
    for (id, draft) in library.claim_due(all)? {
        let label = format!("To {}: {}", draft.to, if draft.subject.is_empty() { "(no subject)" } else { &draft.subject });
        let origin = draft.origin.clone();
        let outcome = library.prepare_send(draft).and_then(|o| o.send());
        match outcome {
            Ok(_) => {
                let _ = library.mark_origin(&origin);
                library.sent_queued(id)?;
                done.sent += 1;
                done.notes.push(format!("Sent (queued): {label}"));
            }
            Err(e) => {
                library.send_failed(id, &e.to_string())?;
                done.failed += 1;
                done.notes.push(format!("Not sent yet ({e}): {label}; tried again later"));
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;

    #[test]
    fn queued_mail_is_claimed_once_and_failures_wait_to_be_tried_again() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let draft = |s: &str| Draft { from: "me@x.com".into(), to: "ann@y.com".into(), subject: s.into(), ..Default::default() };
        let later = library.queue_message(&draft("Later"), now() + 3600).unwrap();
        let due = library.queue_message(&draft("Due"), now() - 5).unwrap();
        let batch = library.queue_message(&draft("With the batch"), 0).unwrap();
        assert_eq!(library.outbox().unwrap().iter().map(|q| q.subject.as_str()).collect::<Vec<_>>(), vec!["With the batch", "Due", "Later"]);

        // only what's due; a second program finds nothing left to claim
        let claimed = library.claim_due(false).unwrap();
        assert_eq!(claimed.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![due]);
        assert!(library.claim_due(false).unwrap().is_empty(), "claimed once");
        assert_eq!(library.outbox().unwrap().iter().find(|q| q.id == due).unwrap().state, "sending");

        // it fails: back in the queue, not before a minute
        library.send_failed(due, "no network").unwrap();
        let q = library.outbox().unwrap().into_iter().find(|q| q.id == due).unwrap();
        assert_eq!((q.state.as_str(), q.tries, q.last_error.as_deref()), ("failed", 1, Some("no network")));
        // Send Queued Messages takes everything queued, but the failed one waits before its next try
        let all: Vec<i64> = library.claim_due(true).unwrap().into_iter().map(|(id, _)| id).collect();
        assert!(all.contains(&batch) && all.contains(&later) && !all.contains(&due), "{all:?}");
        library.sent_queued(batch).unwrap();
        // the user changes their mind: back to a draft
        library.reschedule(later, now() + 60).unwrap();
        assert_eq!(library.unqueue(later).unwrap().subject, "Later");
        assert_eq!(library.outbox().unwrap().len(), 1);
    }
}
