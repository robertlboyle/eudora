//! Calendars and their events. Microsoft 365 / Outlook.com first, through Graph: each signed-in
//! account's calendars, every occurrence of a recurring series as its own row (Graph's
//! calendarView), times in UTC. The download reads the near weeks often and everything now and
//! then (`graph_sync`); the window shows Day / Week / Month / Agenda. The server's own form of
//! each event is kept (`original`).

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::library::{Library, now};

/// How far back "everything" reaches, and how far ahead (calendars are read again daily, so the
/// window moves with time).
pub const EVERYTHING_FROM: i64 = 631_152_000; // 1990-01-01
pub const AHEAD: i64 = 3 * 366 * 86_400;
/// The near weeks, read every 15 minutes: the last month and the next year.
pub const NEAR_BACK: i64 = 31 * 86_400;
pub const NEAR_AHEAD: i64 = 366 * 86_400;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub name: String,
    pub address: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attendee {
    pub name: String,
    pub address: String,
    /// required, optional, resource
    pub kind: String,
    /// none, organizer, accepted, tentativelyAccepted, declined, notResponded
    pub response: String,
}

/// Eudora's reading of an event.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub subject: String,
    /// Unix seconds, UTC. An all-day event runs from midnight UTC of its first day to midnight
    /// UTC after its last: shown on those dates wherever the computer is.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub location: String,
    pub organizer: Person,
    pub attendees: Vec<Attendee>,
    pub is_organizer: bool,
    /// The organizer asked for no answers (Outlook's "Request responses" off): answering is
    /// refused, the meeting just stays on the calendar. (Kept this way round so events stored
    /// before 2026-10-26, and a default event, count as asking for answers.)
    #[serde(default)]
    pub no_answers_asked: bool,
    /// The user's own answer: none, organizer, accepted, tentativelyAccepted, declined, notResponded.
    pub response: String,
    /// free, tentative, busy, oof, workingElsewhere, unknown
    pub show_as: String,
    /// Minutes before the start, when a reminder is on.
    pub reminder: Option<i64>,
    pub categories: Vec<String>,
    pub private: bool,
    pub importance: String,
    pub cancelled: bool,
    /// A Teams / Zoom / web meeting's link.
    pub join_url: Option<String>,
    /// singleInstance, occurrence, exception, seriesMaster
    pub kind: String,
    /// A series' id (for its occurrences and exceptions).
    pub series: Option<String>,
    pub web_link: Option<String>,
    pub preview: String,
    pub ical_uid: String,
}

/// A Graph date and time ({dateTime, timeZone: UTC}) as Unix seconds.
fn utc(v: &Value) -> i64 {
    let text = v["dateTime"].as_str().unwrap_or("");
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
        .map(|t| t.and_utc().timestamp())
        .unwrap_or(0)
}

fn person(v: &Value) -> Person {
    Person { name: v["name"].as_str().unwrap_or("").to_string(), address: v["address"].as_str().unwrap_or("").to_string() }
}

/// An event as Graph gives it (times asked for in UTC).
pub fn from_graph(v: &Value) -> Event {
    let text = |key: &str| v[key].as_str().unwrap_or("").to_string();
    Event {
        subject: text("subject"),
        start: utc(&v["start"]),
        end: utc(&v["end"]),
        all_day: v["isAllDay"].as_bool().unwrap_or(false),
        location: v["location"]["displayName"].as_str().unwrap_or("").to_string(),
        organizer: person(&v["organizer"]["emailAddress"]),
        attendees: v["attendees"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|a| Attendee {
                        name: a["emailAddress"]["name"].as_str().unwrap_or("").to_string(),
                        address: a["emailAddress"]["address"].as_str().unwrap_or("").to_string(),
                        kind: a["type"].as_str().unwrap_or("required").to_string(),
                        response: a["status"]["response"].as_str().unwrap_or("none").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        is_organizer: v["isOrganizer"].as_bool().unwrap_or(false),
        no_answers_asked: v["responseRequested"].as_bool() == Some(false),
        response: v["responseStatus"]["response"].as_str().unwrap_or("none").to_string(),
        show_as: text("showAs"),
        reminder: v["isReminderOn"].as_bool().unwrap_or(false).then(|| v["reminderMinutesBeforeStart"].as_i64().unwrap_or(15)),
        categories: v["categories"].as_array().map(|c| c.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        private: matches!(v["sensitivity"].as_str(), Some("private" | "confidential")),
        importance: text("importance"),
        cancelled: v["isCancelled"].as_bool().unwrap_or(false),
        join_url: v["onlineMeeting"]["joinUrl"].as_str().map(str::to_string).filter(|u| !u.is_empty()),
        kind: v["type"].as_str().unwrap_or("singleInstance").to_string(),
        series: v["seriesMasterId"].as_str().map(str::to_string),
        web_link: v["webLink"].as_str().map(str::to_string),
        preview: text("bodyPreview"),
        ical_uid: text("iCalUId"),
    }
}

/// Outlook's named calendar colors, for calendars with no hex color of their own.
fn named_color(name: &str) -> &'static str {
    match name {
        "lightBlue" => "#4f8fd8",
        "lightGreen" => "#4caf50",
        "lightOrange" => "#f39c32",
        "lightGray" => "#8f9aa5",
        "lightYellow" => "#d4b200",
        "lightTeal" => "#20a39e",
        "lightPink" => "#e45f99",
        "lightBrown" => "#a47551",
        "lightRed" => "#e0534a",
        "maxColor" => "#7a5af5",
        _ => "#3b7dd8",
    }
}

/// Reading an account's calendars: the near weeks, or everything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarJob {
    pub account: i64,
    pub from: i64,
    pub to: i64,
}

/// What it brought back: each calendar (as Graph gives it) with its events in the range.
#[derive(Clone, Debug)]
pub struct CalendarFetched {
    pub account: i64,
    pub from: i64,
    pub to: i64,
    pub calendars: std::result::Result<Vec<(Value, Vec<Value>)>, String>,
}

/// What recording did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EventsRecorded {
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
}

/// A calendar, for the window.
#[derive(Clone, Debug, Serialize)]
pub struct CalendarInfo {
    pub id: i64,
    pub account: i64,
    pub address: String,
    pub name: String,
    /// The color shown: the user's own choice, else the calendar's own.
    pub color: String,
    /// The calendar's own color (from Microsoft or Google), for going back to it.
    pub server_color: String,
    /// The user chose the color here.
    pub own_color: bool,
    pub can_edit: bool,
    pub is_default: bool,
    pub shown: bool,
    pub events: u64,
}

const CALENDAR_COLORS: &str = "calendar-colors";

/// An event in a range (the calendar's views).
#[derive(Clone, Debug, Serialize)]
pub struct EventRow {
    pub id: i64,
    pub calendar: i64,
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub subject: String,
    pub location: String,
    pub show_as: String,
    pub response: String,
    pub cancelled: bool,
    pub kind: String,
    pub private: bool,
    pub join_url: Option<String>,
}

/// One event whole, for its window.
#[derive(Clone, Debug, Serialize)]
pub struct EventView {
    pub id: i64,
    pub calendar: i64,
    pub calendar_name: String,
    pub address: String,
    pub color: String,
    pub event: Event,
}

impl Library {
    /// The calendar jobs for a Microsoft sign-in's own mailbox (its account rows).
    pub fn calendar_jobs(&self, signed_in: &str, from: i64, to: i64) -> Result<Vec<CalendarJob>> {
        let mut stmt = self.conn.prepare("SELECT id FROM accounts WHERE kind = 'graph' AND lower(address) = lower(?1)")?;
        let ids: Vec<i64> = stmt.query_map(params![signed_in], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids.into_iter().map(|account| CalendarJob { account, from, to }).collect())
    }

    /// Stores an account's calendars and their events in a range as fetched: calendars made or
    /// renamed (whether each is shown stays the user's), calendars gone removed with their
    /// events; events added, changed ones (by Graph's changeKey) read again, those in the range
    /// the server no longer has removed.
    pub fn record_calendars(&self, account: i64, from: i64, to: i64, calendars: &[(Value, Vec<Value>)]) -> Result<EventsRecorded> {
        let mut done = EventsRecorded::default();
        let at = now();
        // events with a change of the user's still on its way keep it (an answer isn't undone, a
        // deletion isn't brought back by the server's older copy)
        let waiting = self.events_waiting(account)?;
        self.in_transaction(|| {
            let mut kept_calendars = Vec::new();
            for (cal, events) in calendars {
                let Some(server_id) = cal["id"].as_str() else { continue };
                let color = cal["hexColor"].as_str().filter(|c| c.starts_with('#') && c.len() == 7).map(str::to_string).unwrap_or_else(|| named_color(cal["color"].as_str().unwrap_or("")).to_string());
                self.conn.execute(
                    "INSERT INTO calendars (account, server_id, name, color, can_edit, is_default, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (account, server_id) DO UPDATE SET name = excluded.name, color = excluded.color, can_edit = excluded.can_edit, is_default = excluded.is_default, updated = excluded.updated",
                    params![account, server_id, cal["name"].as_str().unwrap_or("Calendar"), color, cal["canEdit"].as_bool().unwrap_or(true), cal["isDefaultCalendar"].as_bool().unwrap_or(false), at],
                )?;
                let calendar: i64 = self.conn.query_row("SELECT id FROM calendars WHERE account = ?1 AND server_id = ?2", params![account, server_id], |r| r.get(0))?;
                kept_calendars.push(calendar);
                let known: HashMap<String, (i64, Option<String>)> = {
                    let mut stmt = self.conn.prepare("SELECT server_id, id, version FROM events WHERE calendar = ?1 AND start >= ?2 AND start < ?3")?;
                    stmt.query_map(params![calendar, from, to], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?.collect::<rusqlite::Result<_>>()?
                };
                let mut seen = std::collections::HashSet::new();
                for v in events {
                    let Some(id) = v["id"].as_str() else { continue };
                    seen.insert(id.to_string());
                    if waiting.contains(id) {
                        continue;
                    }
                    let version = v["changeKey"].as_str();
                    let e = from_graph(v);
                    let json = serde_json::to_string(&e)?;
                    let original = serde_json::to_string(v)?;
                    let changed = self.conn.execute(
                        "INSERT INTO events (calendar, server_id, version, kind, series, start, end, all_day, subject, location, event, original, updated)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                         ON CONFLICT (calendar, server_id) DO UPDATE SET version = excluded.version, kind = excluded.kind, series = excluded.series,
                           start = excluded.start, end = excluded.end, all_day = excluded.all_day, subject = excluded.subject, location = excluded.location,
                           event = excluded.event, original = excluded.original, updated = excluded.updated
                         WHERE events.version IS NOT excluded.version",
                        params![calendar, id, version, e.kind, e.series, e.start, e.end, e.all_day, e.subject, e.location, json, original, at],
                    )?;
                    match known.get(id) {
                        None => done.added += 1,
                        Some(_) if changed > 0 => done.changed += 1,
                        _ => {}
                    }
                }
                for (server_id, (id, _)) in &known {
                    if !seen.contains(server_id) && !waiting.contains(server_id) {
                        self.conn.execute("DELETE FROM events WHERE id = ?1", params![id])?;
                        done.removed += 1;
                    }
                }
            }
            // calendars gone from the server (deleted in Outlook): with their events
            let gone: Vec<i64> = {
                let mut stmt = self.conn.prepare("SELECT id FROM calendars WHERE account = ?1")?;
                stmt.query_map(params![account], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?.into_iter().filter(|id| !kept_calendars.contains(id)).collect()
            };
            for id in gone {
                done.removed += self.conn.execute("DELETE FROM events WHERE calendar = ?1", params![id])? as u64;
                self.conn.execute("DELETE FROM calendars WHERE id = ?1", params![id])?;
            }
            Ok(())
        })?;
        Ok(done)
    }

    /// Every calendar, with its account's address and how many events it holds.
    pub fn calendars_list(&self) -> Result<Vec<CalendarInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.account, COALESCE(a.address, ''), c.name, COALESCE(c.color, '#3b7dd8'), c.can_edit, c.is_default, c.shown,
                    (SELECT COUNT(*) FROM events e WHERE e.calendar = c.id), c.server_id
             FROM calendars c JOIN accounts a ON a.id = c.account ORDER BY a.position, a.id, c.is_default DESC, c.name",
        )?;
        let chosen = self.calendar_colors()?;
        let rows = stmt.query_map([], |r| {
            let server_color: String = r.get(4)?;
            let own = chosen.get(&r.get::<_, String>(9)?).cloned();
            Ok(CalendarInfo {
                id: r.get(0)?,
                account: r.get(1)?,
                address: r.get(2)?,
                name: r.get(3)?,
                color: own.clone().unwrap_or_else(|| server_color.clone()),
                own_color: own.is_some(),
                server_color,
                can_edit: r.get(5)?,
                is_default: r.get(6)?,
                shown: r.get(7)?,
                events: r.get::<_, i64>(8)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The colors the user chose, by the calendar's server id (so they stay when the calendar is
    /// read again). Kept in Eudora only: Microsoft and Google keep their own.
    fn calendar_colors(&self) -> Result<std::collections::HashMap<String, String>> {
        Ok(self.setting(CALENDAR_COLORS)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    /// A calendar's color here: "#rrggbb", or None for the calendar's own color again.
    pub fn set_calendar_color(&self, calendar: i64, color: Option<&str>) -> Result<()> {
        let server_id: String = self.conn.query_row("SELECT server_id FROM calendars WHERE id = ?1", params![calendar], |r| r.get(0))?;
        let mut colors = self.calendar_colors()?;
        match color {
            Some(c) => {
                let ok = c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit());
                if !ok {
                    return Err(Error::Other(format!("“{c}” isn't a color: #rrggbb")));
                }
                colors.insert(server_id, c.to_ascii_lowercase());
            }
            None => {
                colors.remove(&server_id);
            }
        }
        self.set_setting(CALENDAR_COLORS, &serde_json::to_string(&colors)?)
    }

    /// The account a calendar belongs to.
    pub fn calendar_account(&self, calendar: i64) -> Result<i64> {
        self.conn
            .query_row("SELECT account FROM calendars WHERE id = ?1", params![calendar], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::Other(format!("no calendar {calendar}")))
    }

    /// Shows or hides a calendar in the views (kept on this computer's library).
    pub fn set_calendar_shown(&self, calendar: i64, shown: bool) -> Result<()> {
        self.conn.execute("UPDATE calendars SET shown = ?2 WHERE id = ?1", params![calendar, shown])?;
        Ok(())
    }

    /// The events touching a time range (start before `to`, end after `from`), series masters
    /// left out (their occurrences are rows of their own), in every calendar: the window shows
    /// or hides each.
    pub fn events_between(&self, from: i64, to: i64) -> Result<Vec<EventRow>> {
        // an event starts at most a long time before it ends: an index on start, bounded below
        let longest: i64 = 400 * 86_400;
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, calendar, start, end, all_day, subject, location, event FROM events
             WHERE start < ?2 AND start > ?3 AND end > ?1 AND kind <> 'seriesMaster' ORDER BY start, end DESC",
        )?;
        let rows = stmt.query_map(params![from, to, from - longest], |r| {
            let e: Event = serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default();
            Ok(EventRow {
                id: r.get(0)?,
                calendar: r.get(1)?,
                start: r.get(2)?,
                end: r.get(3)?,
                all_day: r.get(4)?,
                subject: r.get(5)?,
                location: r.get(6)?,
                show_as: e.show_as,
                response: e.response,
                cancelled: e.cancelled,
                kind: e.kind,
                private: e.private,
                join_url: e.join_url,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One event whole.
    pub fn event_get(&self, id: i64) -> Result<EventView> {
        self.conn
            .query_row(
                "SELECT e.id, e.calendar, c.name, COALESCE(a.address, ''), COALESCE(c.color, '#3b7dd8'), e.event
                 FROM events e JOIN calendars c ON c.id = e.calendar JOIN accounts a ON a.id = c.account WHERE e.id = ?1",
                params![id],
                |r| {
                    Ok(EventView {
                        id: r.get(0)?,
                        calendar: r.get(1)?,
                        calendar_name: r.get(2)?,
                        address: r.get(3)?,
                        color: r.get(4)?,
                        event: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::Other(format!("no event {id}")))
    }

    /// The event's row for a change: (account, server id, the event).
    /// Why an invitation can't be answered here, if it can't: it's on someone else's calendar
    /// shared with the user (only they or their delegate may answer: Microsoft refuses with
    /// "access denied"), or its organizer asked for no answers.
    pub fn why_no_answer(&self, calendar: i64, e: &Event) -> Result<Option<String>> {
        let (name, can_edit): (String, bool) = self.conn.query_row("SELECT name, can_edit FROM calendars WHERE id = ?1", params![calendar], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(if !can_edit {
            Some(format!("it's on the “{name}” calendar, which is shared with you: only its owner (or their delegate) can answer it"))
        } else if e.no_answers_asked {
            Some("the organizer didn't ask for answers: it stays on your calendar, and nothing is sent".into())
        } else {
            None
        })
    }

    pub(crate) fn event_for_change(&self, id: i64) -> Result<(i64, String, Event)> {
        let (account, server_id, json): (i64, String, String) = self.conn.query_row(
            "SELECT c.account, e.server_id, e.event FROM events e JOIN calendars c ON c.id = e.calendar WHERE e.id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok((account, server_id, serde_json::from_str(&json)?))
    }

    /// Queues an event change; the calendar it's in goes with it (Google's calls name the
    /// calendar, and the event may be gone here by the time the change is made).
    pub(crate) fn queue_event_change(&self, mut payload: serde_json::Value) -> Result<()> {
        if payload["calendar"].is_null() {
            if let Some(event) = payload["event"].as_i64() {
                let calendar: Option<String> = self.conn.query_row("SELECT c.server_id FROM events e JOIN calendars c ON c.id = e.calendar WHERE e.id = ?1", params![event], |r| r.get(0)).optional()?;
                if let Some(c) = calendar {
                    payload["calendar"] = c.into();
                }
            }
        }
        self.conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'event-change', ?2)", params![now(), payload.to_string()])?;
        Ok(())
    }

    /// Answers an invitation ("accept", "tentative" or "decline"): here at once, then to the
    /// server through the queue, which tells the organizer. A declined meeting leaves the
    /// calendar (as Outlook does).
    pub fn event_respond(&self, id: i64, answer: &str) -> Result<()> {
        self.event_respond_as(id, answer, false)
    }

    /// `event_respond`, for a meeting of a series: with `whole_series`, the answer is for every
    /// meeting in it (sent for the series' own id: the calendar copy holds only its meetings).
    pub fn event_respond_as(&self, id: i64, answer: &str, whole_series: bool) -> Result<()> {
        self.event_respond_with(id, answer, whole_series, "", None)
    }

    /// `event_respond_as` with a message to the organizer and, for tentative or decline, a time
    /// proposed instead (start, end: Unix seconds UTC), as Outlook's Propose New Time.
    pub fn event_respond_with(&self, id: i64, answer: &str, whole_series: bool, comment: &str, proposed: Option<(i64, i64)>) -> Result<()> {
        if proposed.is_some() && answer == "accept" {
            return Err(Error::Other("a new time can be proposed with Tentative or Decline, not Accept".into()));
        }
        if let Some((s, e)) = proposed {
            if e <= s {
                return Err(Error::Other("the time proposed has to end after it starts".into()));
            }
        }
        let iso = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string()).unwrap_or_default();
        let extra = serde_json::json!({
            "comment": if comment.trim().is_empty() { serde_json::Value::Null } else { comment.trim().into() },
            "proposedNewTime": proposed.map(|(s, e)| serde_json::json!({ "start": { "dateTime": iso(s), "timeZone": "UTC" }, "end": { "dateTime": iso(e), "timeZone": "UTC" } })),
        });
        let graph = match answer {
            "accept" => "accept",
            "tentative" => "tentativelyAccept",
            "decline" => "decline",
            other => return Err(Error::Other(format!("unknown answer {other}: accept, tentative or decline"))),
        };
        let (account, server_id, mut e) = self.event_for_change(id)?;
        if e.is_organizer {
            return Err(Error::Other("it's your own meeting: there's no invitation to answer".into()));
        }
        // a series answered whole: every meeting of it here, the series' id on the server
        let calendar: i64 = self.conn.query_row("SELECT calendar FROM events WHERE id = ?1", params![id], |r| r.get(0))?;
        // someone else's calendar: refused; no answers asked: kept here only (below)
        if !e.no_answers_asked {
            if let Some(why) = self.why_no_answer(calendar, &e)? {
                return Err(Error::Other(why));
            }
        } else if !self.conn.query_row("SELECT can_edit FROM calendars WHERE id = ?1", params![calendar], |r| r.get::<_, bool>(0))? {
            return Err(Error::Other("it's on a calendar shared with you: only its owner can answer it".into()));
        }
        let series: Option<String> = if e.kind == "seriesMaster" { Some(server_id.clone()) } else if whole_series { e.series.clone() } else { None };
        let target = series.clone().unwrap_or_else(|| server_id.clone());
        self.in_transaction(|| {
            if graph == "decline" {
                self.conn.execute("DELETE FROM events WHERE id = ?1", params![id])?;
                if let Some(s) = &series {
                    self.conn.execute("DELETE FROM events WHERE calendar = ?1 AND (series = ?2 OR server_id = ?2)", params![calendar, s])?;
                }
            } else {
                e.response = if graph == "accept" { "accepted" } else { "tentativelyAccepted" }.into();
                e.show_as = if graph == "accept" { "busy" } else { "tentative" }.into();
                self.conn.execute("UPDATE events SET event = ?2 WHERE id = ?1", params![id, serde_json::to_string(&e)?])?;
                if let Some(s) = &series {
                    self.conn.execute(
                        "UPDATE events SET event = json_set(event, '$.response', ?3, '$.show_as', ?4) WHERE calendar = ?1 AND (series = ?2 OR server_id = ?2)",
                        params![calendar, s, e.response, e.show_as],
                    )?;
                }
            }
            // (an organizer who asked for no answers: Microsoft refuses any; the answer stays
            // here, and a decline takes the meeting off the calendar quietly, as Outlook does)
            if e.no_answers_asked {
                return if graph == "decline" { self.queue_event_change(serde_json::json!({ "event": id, "account": account, "server_id": target, "action": "delete" })) } else { Ok(()) };
            }
            self.queue_event_change(serde_json::json!({ "event": id, "account": account, "server_id": target, "action": "respond", "answer": graph, "patch": extra }))
        })
    }

    /// Changes an event's private mark, how it shows (free, busy…) or its categories: here at
    /// once, then on the server.
    pub fn event_update(&self, id: i64, private: Option<bool>, show_as: Option<&str>, categories: Option<&[String]>) -> Result<()> {
        let (account, server_id, mut e) = self.event_for_change(id)?;
        let mut patch = serde_json::Map::new();
        if let Some(p) = private {
            e.private = p;
            patch.insert("sensitivity".into(), (if p { "private" } else { "normal" }).into());
        }
        if let Some(s) = show_as {
            if !["free", "tentative", "busy", "oof", "workingElsewhere"].contains(&s) {
                return Err(Error::Other(format!("show as {s}? free, tentative, busy, oof or workingElsewhere")));
            }
            e.show_as = s.to_string();
            patch.insert("showAs".into(), s.into());
        }
        if let Some(c) = categories {
            e.categories = c.to_vec();
            patch.insert("categories".into(), c.into());
        }
        if patch.is_empty() {
            return Ok(());
        }
        self.in_transaction(|| {
            self.conn.execute("UPDATE events SET event = ?2 WHERE id = ?1", params![id, serde_json::to_string(&e)?])?;
            self.queue_event_change(serde_json::json!({ "event": id, "account": account, "server_id": server_id, "action": "update", "patch": patch }))
        })
    }

    /// Deletes an event, the way Outlook does: a meeting someone else organized is declined
    /// (the organizer is told), one the user organized with people invited is canceled (they're
    /// told), anything else deleted. Gone here at once; returns what was done.
    pub fn event_delete(&self, id: i64) -> Result<&'static str> {
        self.event_delete_as(id, false)
    }

    /// `event_delete`, for one meeting of a series or (`whole_series`) every meeting of it: the
    /// series' own id goes to the server (Microsoft and Google delete, cancel or decline every
    /// meeting of it), and every meeting of it leaves this calendar at once.
    pub fn event_delete_as(&self, id: i64, whole_series: bool) -> Result<&'static str> {
        let (account, mut server_id, e) = self.event_for_change(id)?;
        let series = if e.kind == "seriesMaster" { Some(server_id.clone()) } else if whole_series { e.series.clone() } else { None };
        if whole_series && series.is_none() {
            return Err(Error::Other("it isn't part of a series".into()));
        }
        if let Some(s) = &series {
            server_id = s.clone();
        }
        let (calendar_name, can_edit): (String, bool) = self.conn.query_row("SELECT c.name, c.can_edit FROM events e JOIN calendars c ON c.id = e.calendar WHERE e.id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if !can_edit {
            return Err(Error::Other(format!("it's on the “{calendar_name}” calendar, which is shared with you: only its owner can change it")));
        }
        // (no answers asked: declining is refused, so it just leaves the calendar)
        // (a canceled meeting can't be declined: it's simply removed)
        let (action, said) = if !e.is_organizer && !e.organizer.address.is_empty() && !e.attendees.is_empty() && !e.no_answers_asked && !e.cancelled {
            ("decline", "declined")
        } else if e.is_organizer && !e.attendees.is_empty() && !e.cancelled {
            ("cancel", "canceled")
        } else {
            ("delete", "deleted")
        };
        self.in_transaction(|| {
            let payload = if action == "decline" {
                serde_json::json!({ "event": id, "account": account, "server_id": server_id, "action": "respond", "answer": "decline" })
            } else {
                serde_json::json!({ "event": id, "account": account, "server_id": server_id, "action": action })
            };
            self.queue_event_change(payload)?;
            if let Some(s) = &series {
                self.conn.execute("DELETE FROM events WHERE calendar = (SELECT calendar FROM events WHERE id = ?1) AND (series = ?2 OR server_id = ?2)", params![id, s])?;
            }
            self.conn.execute("DELETE FROM events WHERE id = ?1", params![id])?;
            Ok(())
        })?;
        Ok(said)
    }

    /// Meetings someone else organized and canceled, not over yet, taken off the calendar: here
    /// at once, and on the server quietly (Outlook's Remove from Calendar: a canceled meeting
    /// can't be answered). Meetings already over are left as they are (nothing years old is
    /// deleted at once). Returns how many.
    pub fn remove_canceled_meetings(&self, account: i64) -> Result<u64> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id FROM events e JOIN calendars c ON c.id = e.calendar
             WHERE c.account = ?1 AND c.can_edit AND e.kind <> 'seriesMaster' AND e.end > ?2
               AND json_extract(e.event, '$.cancelled') = 1 AND json_extract(e.event, '$.is_organizer') = 0",
        )?;
        let ids: Vec<i64> = stmt.query_map(params![account, now()], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut removed = 0;
        for id in ids {
            if self.event_delete_as(id, false).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Makes an event in a calendar (`graph`: the event as Graph takes it, times in UTC): here
    /// at once under a temporary id, on the server through the queue (it gets the server's id
    /// then). Returns its id here.
    pub fn create_event(&self, calendar: i64, graph: &serde_json::Value) -> Result<i64> {
        let (account, server_calendar): (i64, String) = self.conn.query_row("SELECT account, server_id FROM calendars WHERE id = ?1", params![calendar], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut shown = graph.clone();
        // (read back as Graph would give it: the same reader for both)
        shown["type"] = "singleInstance".into();
        shown["isOrganizer"] = true.into();
        shown["responseStatus"] = serde_json::json!({ "response": "organizer" });
        let e = from_graph(&shown);
        let temp = format!("local:{}", crate::graph::random(8)?.iter().map(|b| format!("{b:02x}")).collect::<String>());
        self.in_transaction(|| {
            self.conn.execute(
                "INSERT INTO events (calendar, server_id, version, kind, series, start, end, all_day, subject, location, event, original, updated)
                 VALUES (?1, ?2, NULL, 'singleInstance', NULL, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9)",
                params![calendar, temp, e.start, e.end, e.all_day, e.subject, e.location, serde_json::to_string(&e)?, now()],
            )?;
            let id = self.conn.last_insert_rowid();
            self.queue_event_change(serde_json::json!({ "event": id, "account": account, "server_id": temp, "action": "create", "calendar": server_calendar, "patch": graph }))?;
            Ok(id)
        })
    }

    /// The server ids of an account's events with a change still on its way: the calendar read
    /// from the server leaves them as they are here.
    fn events_waiting(&self, account: i64) -> Result<std::collections::HashSet<String>> {
        // (a change given up on no longer holds its event: the server's copy is read again)
        let mut stmt = self.conn.prepare(
            "SELECT json_extract(payload, '$.server_id') FROM pending_ops WHERE kind = 'event-change' AND json_extract(payload, '$.account') = ?1 AND attempts < ?2",
        )?;
        let ids = stmt.query_map(params![account, crate::server_changes::GIVE_UP], |r| r.get::<_, Option<String>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids.into_iter().flatten().collect())
    }

    /// Events whose subject, place or people match these words (newest first, at most `limit`).
    pub fn search_events(&self, words: &str, limit: usize) -> Result<Vec<EventRow>> {
        let like = format!("%{}%", words.trim().replace('%', "").replace('_', ""));
        let mut stmt = self.conn.prepare(
            "SELECT id FROM events WHERE kind <> 'seriesMaster' AND (subject LIKE ?1 OR location LIKE ?1 OR event LIKE ?1) ORDER BY start DESC LIMIT ?2",
        )?;
        let ids: Vec<i64> = stmt.query_map(params![like, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for id in ids {
            let v = self.event_get(id)?;
            let e = v.event;
            out.push(EventRow {
                id,
                calendar: v.calendar,
                start: e.start,
                end: e.end,
                all_day: e.all_day,
                subject: e.subject,
                location: e.location,
                show_as: e.show_as,
                response: e.response,
                cancelled: e.cancelled,
                kind: e.kind,
                private: e.private,
                join_url: e.join_url,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: &str, key: &str, subject: &str, start: &str, end: &str) -> Value {
        json!({
            "id": id, "changeKey": key, "subject": subject, "type": "singleInstance",
            "start": { "dateTime": start, "timeZone": "UTC" }, "end": { "dateTime": end, "timeZone": "UTC" },
            "isAllDay": false, "location": { "displayName": "Room 1" },
            "organizer": { "emailAddress": { "name": "Ann", "address": "ann@x.com" } },
            "attendees": [{ "type": "required", "status": { "response": "accepted" }, "emailAddress": { "name": "Bob", "address": "bob@x.com" } }],
            "responseStatus": { "response": "organizer" }, "showAs": "busy", "isReminderOn": true, "reminderMinutesBeforeStart": 10,
            "onlineMeeting": { "joinUrl": "https://teams.example/join" }
        })
    }

    #[test]
    fn graph_events_are_read() {
        let e = from_graph(&event("1", "a", "Standup", "2026-10-19T14:00:00.0000000", "2026-10-19T14:15:00.0000000"));
        assert_eq!(e.subject, "Standup");
        assert_eq!(e.end - e.start, 15 * 60);
        assert_eq!(e.start, chrono::DateTime::parse_from_rfc3339("2026-10-19T14:00:00Z").unwrap().timestamp());
        assert_eq!(e.location, "Room 1");
        assert_eq!(e.organizer.address, "ann@x.com");
        assert_eq!(e.attendees[0].response, "accepted");
        assert_eq!(e.reminder, Some(10));
        assert_eq!(e.join_url.as_deref(), Some("https://teams.example/join"));
    }

    #[test]
    fn calendars_are_recorded_changed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::Silent).unwrap();
        let account = crate::db::graph_account(&library.conn, "graph", "me@x.com", "Me", 0).unwrap();
        let cal = json!({ "id": "C1", "name": "Calendar", "hexColor": "", "color": "lightGreen", "canEdit": true, "isDefaultCalendar": true });
        let (from, to) = (0, 4_000_000_000);
        let a = event("E1", "k1", "One", "2026-10-19T14:00:00", "2026-10-19T15:00:00");
        let b = event("E2", "k1", "Two", "2026-10-20T14:00:00", "2026-10-20T15:00:00");
        let r = library.record_calendars(account, from, to, &[(cal.clone(), vec![a.clone(), b.clone()])]).unwrap();
        assert_eq!(r, EventsRecorded { added: 2, changed: 0, removed: 0 });
        let cals = library.calendars_list().unwrap();
        assert_eq!((cals.len(), cals[0].color.as_str(), cals[0].events), (1, "#4caf50", 2));
        // the same again: nothing; one changed, the other gone
        assert_eq!(library.record_calendars(account, from, to, &[(cal.clone(), vec![a.clone(), b])]).unwrap(), EventsRecorded::default());
        let a2 = event("E1", "k2", "One, moved", "2026-10-19T16:00:00", "2026-10-19T17:00:00");
        assert_eq!(library.record_calendars(account, from, to, &[(cal.clone(), vec![a2])]).unwrap(), EventsRecorded { added: 0, changed: 1, removed: 1 });
        let day = chrono::DateTime::parse_from_rfc3339("2026-10-19T00:00:00Z").unwrap().timestamp();
        let rows = library.events_between(day, day + 86_400).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject, "One, moved");
        assert_eq!(library.event_get(rows[0].id).unwrap().event.attendees.len(), 1);
        assert_eq!(library.search_events("moved", 10).unwrap().len(), 1);
        // a calendar hidden stays hidden when read again; a calendar gone goes with its events
        library.set_calendar_shown(cals[0].id, false).unwrap();
        library.record_calendars(account, from, to, &[(cal, vec![])]).unwrap();
        assert!(!library.calendars_list().unwrap()[0].shown);
        library.record_calendars(account, from, to, &[]).unwrap();
        assert!(library.calendars_list().unwrap().is_empty());
    }

    #[test]
    fn answers_changes_and_deletions_wait_for_the_server_and_stay() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::Silent).unwrap();
        let account = crate::db::graph_account(&library.conn, "graph", "me@x.com", "Me", 0).unwrap();
        let cal = json!({ "id": "C1", "name": "Calendar", "hexColor": "#112233", "canEdit": true });
        let (from, to) = (0, 4_000_000_000);
        let a = event("E1", "k1", "Invited", "2026-10-19T14:00:00", "2026-10-19T15:00:00");
        let b = event("E2", "k1", "Another", "2026-10-20T14:00:00", "2026-10-20T15:00:00");
        library.record_calendars(account, from, to, &[(cal.clone(), vec![a.clone(), b.clone()])]).unwrap();
        let ids: Vec<i64> = library.events_between(from, to).unwrap().iter().map(|r| r.id).collect();
        library.event_respond(ids[0], "tentative").unwrap();
        library.event_update(ids[0], Some(true), Some("oof"), None).unwrap();
        assert_eq!(library.event_delete(ids[1]).unwrap(), "declined", "someone else's meeting is declined");
        let e = library.event_get(ids[0]).unwrap().event;
        assert_eq!((e.response.as_str(), e.show_as.as_str(), e.private), ("tentativelyAccepted", "oof", true));
        let jobs = crate::server_changes::event_jobs(&library, "me@x.com", 10).unwrap();
        assert_eq!(jobs.iter().map(|j| (j.action.as_str(), j.answer.as_deref())).collect::<Vec<_>>(), vec![("respond", Some("tentativelyAccept")), ("update", None), ("respond", Some("decline"))]);
        // the server's older copies don't undo them meanwhile
        library.record_calendars(account, from, to, &[(cal, vec![a, b])]).unwrap();
        let rows = library.events_between(from, to).unwrap();
        assert_eq!(rows.len(), 1, "the declined one stays gone");
        assert_eq!(rows[0].response, "tentativelyAccepted");
        assert!(library.event_respond(ids[0], "maybe").is_err());
    }

    /// One meeting of a series, or the whole series (the series' id to the server).
    #[test]
    fn a_whole_series_is_deleted_by_its_series_id() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::Silent).unwrap();
        let account = crate::db::graph_account(&library.conn, "graph", "me@x.com", "Me", 0).unwrap();
        let cal = json!({ "id": "C1", "name": "Calendar", "hexColor": "#112233", "canEdit": true });
        let (from, to) = (0, 4_000_000_000);
        let weekly: Vec<serde_json::Value> = (0..4)
            .map(|n| {
                let mut e = event(&format!("W{n}"), "k1", "Weekly", &format!("2026-11-{:02}T14:00:00", 2 + 7 * n), &format!("2026-11-{:02}T15:00:00", 2 + 7 * n));
                e["type"] = "occurrence".into();
                e["seriesMasterId"] = "S1".into();
                e["isOrganizer"] = true.into();
                e["attendees"] = json!([]);
                e
            })
            .collect();
        let other = event("X1", "k1", "Other", "2026-11-03T14:00:00", "2026-11-03T15:00:00");
        library.record_calendars(account, from, to, &[(cal, [weekly, vec![other]].concat())]).unwrap();
        let rows = library.events_between(from, to).unwrap();
        let weeklies: Vec<i64> = rows.iter().filter(|r| r.subject == "Weekly").map(|r| r.id).collect();
        assert_eq!(weeklies.len(), 4);
        // one meeting: only it
        library.event_delete_as(weeklies[0], false).unwrap();
        assert_eq!(library.events_between(from, to).unwrap().iter().filter(|r| r.subject == "Weekly").count(), 3);
        // the series: every meeting of it, the series' id to the server; the other event stays
        library.event_delete_as(weeklies[1], true).unwrap();
        let left: Vec<String> = library.events_between(from, to).unwrap().into_iter().map(|r| r.subject).collect();
        assert_eq!(left, vec!["Other"]);
        let jobs = crate::server_changes::event_jobs(&library, "me@x.com", 10).unwrap();
        assert_eq!(jobs.iter().map(|j| j.server_id.as_str()).collect::<Vec<_>>(), vec!["W0", "S1"]);
        // an event outside a series can't be deleted as one
        let other = library.events_between(from, to).unwrap()[0].id;
        assert!(library.event_delete_as(other, true).is_err());
    }

    /// A calendar's own color, the user's choice over it (kept when the calendar is read again),
    /// and back to its own.
    #[test]
    fn a_calendar_takes_the_users_color_and_gives_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::Silent).unwrap();
        let account = crate::db::graph_account(&library.conn, "graph", "me@x.com", "Me", 0).unwrap();
        let cal = json!({ "id": "C1", "name": "Calendar", "hexColor": "#112233", "canEdit": true });
        library.record_calendars(account, 0, 4_000_000_000, &[(cal.clone(), vec![])]).unwrap();
        let id = library.calendars_list().unwrap()[0].id;
        library.set_calendar_color(id, Some("#D13438")).unwrap();
        library.record_calendars(account, 0, 4_000_000_000, &[(cal, vec![])]).unwrap();
        let c = &library.calendars_list().unwrap()[0];
        assert_eq!((c.color.as_str(), c.server_color.as_str(), c.own_color), ("#d13438", "#112233", true));
        assert!(library.set_calendar_color(id, Some("red")).is_err());
        library.set_calendar_color(id, None).unwrap();
        assert_eq!(library.calendars_list().unwrap()[0].color, "#112233");
    }

    /// Someone else's canceled meeting: deleting it removes it, never declines it.
    #[test]
    fn a_canceled_meeting_is_removed_not_declined() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::Silent).unwrap();
        let account = crate::db::graph_account(&library.conn, "graph", "me@x.com", "Me", 0).unwrap();
        let cal = json!({ "id": "C1", "name": "Calendar", "hexColor": "#112233", "canEdit": true });
        let mut e = event("E1", "k1", "Canceled: Review", "2026-11-09T14:00:00", "2026-11-09T15:00:00");
        e["isCancelled"] = true.into();
        e["isOrganizer"] = false.into();
        library.record_calendars(account, 0, 4_000_000_000, &[(cal, vec![e])]).unwrap();
        let id = library.events_between(0, 4_000_000_000).unwrap()[0].id;
        // (Microsoft refuses any answer to a canceled meeting)
        assert_eq!(library.event_delete(id).unwrap(), "deleted");
        let jobs = crate::server_changes::event_jobs(&library, "me@x.com", 10).unwrap();
        assert_eq!(jobs.iter().map(|j| j.action.as_str()).collect::<Vec<_>>(), vec!["delete"]);
        // and taken off by itself when the calendar is read: one ahead goes, one over stays
        let cal = json!({ "id": "C1", "name": "Calendar", "hexColor": "#112233", "canEdit": true });
        let canceled = |id: &str, start: &str, end: &str| {
            let mut e = event(id, "k1", "Canceled: Sync", start, end);
            e["isCancelled"] = true.into();
            e["isOrganizer"] = false.into();
            e
        };
        library.record_calendars(account, 0, 4_000_000_000, &[(cal, vec![canceled("E2", "2099-01-05T14:00:00", "2099-01-05T15:00:00"), canceled("E3", "2001-01-05T14:00:00", "2001-01-05T15:00:00")])]).unwrap();
        assert_eq!(library.remove_canceled_meetings(account).unwrap(), 1);
        let left: Vec<String> = library.events_between(0, 4_102_444_800).unwrap().into_iter().map(|r| r.subject).collect();
        assert_eq!(left, vec!["Canceled: Sync"], "the one already over stays");
        assert_eq!(crate::server_changes::event_jobs(&library, "me@x.com", 10).unwrap().len(), 2);
    }

    /// No answers asked, or someone else's calendar: Microsoft refuses an answer, so none goes.
    #[test]
    fn invitations_that_cant_be_answered_send_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::Silent).unwrap();
        let account = crate::db::graph_account(&library.conn, "graph", "me@x.com", "Me", 0).unwrap();
        let mine = json!({ "id": "C1", "name": "Calendar", "hexColor": "#112233", "canEdit": true });
        let shared = json!({ "id": "C2", "name": "Sam Lee", "hexColor": "#445566", "canEdit": false });
        let (from, to) = (0, 4_000_000_000);
        let mut quiet = event("E1", "k1", "Workshop", "2026-11-05T14:00:00", "2026-11-05T15:00:00");
        quiet["responseRequested"] = false.into();
        let quiet2 = { let mut q = quiet.clone(); q["id"] = "E3".into(); q["subject"] = "Workshop 2".into(); q };
        let alex = event("E2", "k1", "LOI discussion", "2026-11-06T14:00:00", "2026-11-06T15:00:00");
        library.record_calendars(account, from, to, &[(mine, vec![quiet, quiet2]), (shared, vec![alex])]).unwrap();
        let id = |s: &str| library.events_between(from, to).unwrap().into_iter().find(|r| r.subject == s).unwrap().id;
        let (workshop, workshop2, loi) = (id("Workshop"), id("Workshop 2"), id("LOI discussion"));
        assert!(library.event_get(workshop).unwrap().event.no_answers_asked);
        // accepted here, nothing sent; declined = taken off quietly
        library.event_respond(workshop, "accept").unwrap();
        assert_eq!(library.event_get(workshop).unwrap().event.response, "accepted");
        library.event_respond(workshop2, "decline").unwrap();
        assert_eq!(library.event_delete(workshop).unwrap(), "deleted");
        let jobs = crate::server_changes::event_jobs(&library, "me@x.com", 10).unwrap();
        assert_eq!(jobs.iter().map(|j| j.action.as_str()).collect::<Vec<_>>(), vec!["delete", "delete"], "no answers, only quiet removals");
        // someone else's calendar: refused, with why
        let why = library.event_respond(loi, "accept").unwrap_err().to_string();
        assert!(why.contains("Sam Lee"), "{why}");
        assert!(library.event_delete(loi).is_err());
    }
}
