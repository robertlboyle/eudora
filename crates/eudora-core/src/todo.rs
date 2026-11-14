//! To Do.
//!
//! Tasks live here and in **Microsoft To Do** (the default Microsoft account's), so Outlook, the
//! To Do app and Eudora on every computer share one list. Flagging a Microsoft 365 message sets
//! Outlook's flag (Outlook then lists it in To Do's "Flagged email"; that task is matched back to
//! the message); flagging any other message makes a task in the default list carrying its
//! Message-ID, so another computer's Eudora opens the same message. Changes made here are queued
//! (`pending_ops` "todo-change") and carried out by the download; Microsoft To Do is read back
//! every 5 minutes. Twos (its unofficial API, the user's choice) can take a task too.

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::library::{Library, now};

/// A task, for the window.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Todo {
    pub id: i64,
    pub title: String,
    pub notes: String,
    /// Unix seconds (midnight UTC of the day it's due), when it has one.
    pub due: Option<i64>,
    pub done: bool,
    pub important: bool,
    /// The message it's about (here), and its Message-ID (any computer).
    pub message: Option<i64>,
    pub message_id: Option<String>,
    /// Microsoft To Do: its list's name ("Tasks", "Flagged email"…), and whether it's there yet.
    pub list: String,
    pub synced: bool,
    pub created: i64,
}

const LISTS_SETTING: &str = "todo-lists";
const SHOWN_SETTING: &str = "todo-lists-shown";

/// A To Do list, for showing or hiding it.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TodoList {
    pub id: String,
    pub name: String,
    /// The user's own (else shared with them by someone else).
    pub owner: bool,
    /// Shared with others (the user's own list they shared, or another's).
    pub shared: bool,
    pub default: bool,
    pub flagged: bool,
    #[serde(default)]
    pub shown: bool,
    #[serde(default)]
    pub open: u64,
    /// Its color in Eudora: the user's choice, else one of the palette's, picked from its id.
    #[serde(default)]
    pub color: String,
    /// The user chose the color.
    #[serde(default)]
    pub own_color: bool,
}

/// The colors lists start with (the calendar's palette in the window); Microsoft To Do gives
/// lists none.
const PALETTE: [&str; 14] = ["#d13438", "#e8590c", "#f2c94c", "#7fba00", "#2e9e4f", "#00a3a3", "#0078d4", "#3b5bdb", "#8764b8", "#c239b3", "#e3008c", "#8a6d3b", "#69797e", "#4a4a4a"];
const COLORS_SETTING: &str = "todo-list-colors";

/// A list's first color: the default list blue, flagged email orange, the others from their id.
fn first_color(l: &TodoList) -> String {
    if l.default {
        return "#0078d4".into();
    }
    if l.flagged {
        return "#e8590c".into();
    }
    let hash = l.id.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[hash as usize % PALETTE.len()].into()
}

/// Microsoft To Do read back: each list (id, name, well-known name) with its tasks.
#[derive(Clone, Debug)]
pub struct TodoFetched {
    pub account: i64,
    pub lists: std::result::Result<Vec<(Value, Vec<Value>)>, String>,
}

/// A change going to Microsoft To Do.
#[derive(Clone, Debug)]
pub struct TodoJob {
    pub op: i64,
    pub todo: i64,
    /// create, update, delete
    pub action: String,
    /// The list (its server id; for a new task, the default list: found by the client).
    pub list: Option<String>,
    pub server_id: Option<String>,
    pub task: Value,
}

/// What recording did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TodoRecorded {
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
}

/// A Graph date ({dateTime, timeZone}) as the Unix midnight of its day.
fn day_of(v: &Value) -> Option<i64> {
    let text = v["dateTime"].as_str()?;
    chrono::NaiveDate::parse_from_str(&text[..text.len().min(10)], "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
}

/// The task as Graph takes it.
fn to_graph(t: &Todo, subject: Option<&str>) -> Value {
    let mut v = json!({
        "title": t.title,
        "status": if t.done { "completed" } else { "notStarted" },
        "importance": if t.important { "high" } else { "normal" },
        "body": { "contentType": "text", "content": t.notes },
    });
    v["dueDateTime"] = match t.due {
        Some(d) => json!({ "dateTime": chrono::DateTime::from_timestamp(d, 0).unwrap_or_default().format("%Y-%m-%dT00:00:00").to_string(), "timeZone": "UTC" }),
        None => Value::Null,
    };
    if let Some(mid) = &t.message_id {
        v["linkedResources"] = json!([{ "applicationName": "Eudora", "displayName": subject.unwrap_or(&t.title), "externalId": mid, "webUrl": format!("https://eudora.invalid/message/{}", mid.trim_matches(['<', '>'])) }]);
    }
    v
}

const TODO_COLUMNS: &str = "id, title, notes, due, done, important, message, message_id, list_name, server_id IS NOT NULL, created";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Todo> {
    Ok(Todo {
        id: r.get(0)?,
        title: r.get(1)?,
        notes: r.get(2)?,
        due: r.get(3)?,
        done: r.get(4)?,
        important: r.get(5)?,
        message: r.get(6)?,
        message_id: r.get(7)?,
        list: r.get(8)?,
        synced: r.get(9)?,
        created: r.get(10)?,
    })
}

impl Library {
    /// Every task not deleted: open ones by due date, then done ones (the last month's).
    /// Lists hidden (`todo_lists`) are left out.
    pub fn todos(&self) -> Result<Vec<Todo>> {
        let hidden = self.hidden_lists()?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TODO_COLUMNS}, list_id FROM todos WHERE NOT deleted AND (NOT done OR done_at > ?1)
             ORDER BY done, due IS NULL, due, important DESC, created DESC"
        ))?;
        let rows = stmt.query_map(params![now() - 31 * 86_400], |r| Ok((row(r)?, r.get::<_, Option<String>>(11)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (todo, list) = r?;
            if list.is_none_or(|l| !hidden.contains(&l)) {
                out.push(todo);
            }
        }
        Ok(out)
    }

    pub fn todo(&self, id: i64) -> Result<Todo> {
        self.conn
            .query_row(&format!("SELECT {TODO_COLUMNS} FROM todos WHERE id = ?1"), params![id], row)
            .optional()?
            .ok_or_else(|| Error::Other(format!("no task {id}")))
    }

    /// How many tasks are open (the sidebar's number).
    pub fn todos_open(&self) -> Result<u64> {
        Ok(self.todos()?.iter().filter(|t| !t.done).count() as u64)
    }

    fn queue_todo(&self, todo: i64, action: &str) -> Result<()> {
        // To Do kept local (it follows the calendar switch of the Microsoft account it syncs
        // through: `local_only`): the change stays here
        if self.todo_local_only()? {
            return Ok(());
        }
        // one waiting change per task (the latest state goes); a deletion replaces it
        self.conn.execute("DELETE FROM pending_ops WHERE kind = 'todo-change' AND json_extract(payload, '$.todo') = ?1 AND json_extract(payload, '$.action') = 'update'", params![todo])?;
        let creating: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pending_ops WHERE kind = 'todo-change' AND json_extract(payload, '$.todo') = ?1 AND json_extract(payload, '$.action') = 'create')",
            params![todo],
            |r| r.get(0),
        )?;
        match (action, creating) {
            ("update", true) => return Ok(()), // goes with its creation
            ("delete", true) => {
                self.conn.execute("DELETE FROM pending_ops WHERE kind = 'todo-change' AND json_extract(payload, '$.todo') = ?1", params![todo])?;
                self.conn.execute("DELETE FROM todos WHERE id = ?1", params![todo])?;
                return Ok(());
            }
            _ => {}
        }
        self.conn.execute("INSERT INTO pending_ops (created, kind, payload) VALUES (?1, 'todo-change', ?2)", params![now(), json!({ "todo": todo, "action": action }).to_string()])?;
        Ok(())
    }

    /// A new task (to Microsoft To Do's default list).
    pub fn todo_add(&self, title: &str, notes: &str, due: Option<i64>, message: Option<i64>) -> Result<i64> {
        let title = title.trim();
        if title.is_empty() {
            return Err(Error::Other("a task needs a title".into()));
        }
        let message_id: Option<String> = match message {
            Some(m) => self.conn.query_row("SELECT message_id FROM messages WHERE id = ?1", params![m], |r| r.get(0)).optional()?,
            None => None,
        };
        self.in_transaction(|| {
            self.conn.execute(
                "INSERT INTO todos (title, notes, due, message, message_id, list_name, created, updated) VALUES (?1, ?2, ?3, ?4, ?5, 'Tasks', ?6, ?6)",
                params![title, notes, due, message, message_id, now()],
            )?;
            let id = self.conn.last_insert_rowid();
            self.queue_todo(id, "create")?;
            Ok(id)
        })
    }

    /// Changes a task (any of its title, notes, due date, done, importance).
    pub fn todo_update(&self, id: i64, title: Option<&str>, notes: Option<&str>, due: Option<Option<i64>>, done: Option<bool>, important: Option<bool>) -> Result<()> {
        let mut t = self.todo(id)?;
        if let Some(x) = title.map(str::trim).filter(|x| !x.is_empty()) {
            t.title = x.to_string();
        }
        if let Some(x) = notes {
            t.notes = x.to_string();
        }
        if let Some(x) = due {
            t.due = x;
        }
        if let Some(x) = important {
            t.important = x;
        }
        let finished = done.is_some_and(|d| d != t.done);
        if let Some(x) = done {
            t.done = x;
        }
        self.in_transaction(|| {
            self.conn.execute(
                "UPDATE todos SET title = ?2, notes = ?3, due = ?4, done = ?5, important = ?6, done_at = CASE WHEN ?5 THEN COALESCE(done_at, ?7) ELSE NULL END, updated = ?7 WHERE id = ?1",
                params![id, t.title, t.notes, t.due, t.done, t.important, now()],
            )?;
            // a flagged message's task done = its flag taken off (Outlook does the same)
            if finished && t.done && t.list == "Flagged email" {
                if let Some(m) = t.message {
                    self.set_flagged(&[m], false)?;
                }
            }
            if t.list == "Flagged email" && self.conn.query_row("SELECT server_id IS NULL FROM todos WHERE id = ?1", params![id], |r| r.get::<_, bool>(0))? {
                return Ok(()); // (lives through the message's flag until Outlook lists it)
            }
            self.queue_todo(id, "update")
        })
    }

    /// Deletes a task (from Microsoft To Do too). A flagged message's task takes its flag off.
    pub fn todo_delete(&self, id: i64) -> Result<()> {
        let t = self.todo(id)?;
        self.in_transaction(|| {
            if t.list == "Flagged email" {
                if let Some(m) = t.message {
                    self.set_flagged(&[m], false)?;
                }
            }
            self.conn.execute("UPDATE todos SET deleted = 1, updated = ?2 WHERE id = ?1", params![id, now()])?;
            if self.conn.query_row("SELECT server_id IS NOT NULL FROM todos WHERE id = ?1", params![id], |r| r.get::<_, bool>(0))? {
                self.queue_todo(id, "delete")?;
            } else {
                self.conn.execute("DELETE FROM pending_ops WHERE kind = 'todo-change' AND json_extract(payload, '$.todo') = ?1", params![id])?;
            }
            Ok(())
        })
    }

    /// Flags messages as To Do items, or takes the flag off. A Microsoft 365 message gets
    /// Outlook's flag (its task comes back from To Do's "Flagged email"; until then a task here
    /// stands for it); any other message gets a task of its own. Returns the tasks made.
    pub fn flag_messages(&self, ids: &[i64], on: bool) -> Result<Vec<i64>> {
        let mut made = Vec::new();
        self.in_transaction(|| {
            for &id in ids {
                let (subject, outlook): (String, bool) = self.conn.query_row(
                    "SELECT subject, EXISTS (SELECT 1 FROM locations l JOIN folders f ON f.id = l.folder JOIN accounts a ON a.id = f.account
                       WHERE l.message = m.id AND l.server_id IS NOT NULL AND a.kind LIKE 'graph%' AND f.path NOT LIKE 'Online Archive/%')
                     FROM messages m WHERE m.id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let open: Vec<i64> = {
                    let mut stmt = self.conn.prepare_cached("SELECT id FROM todos WHERE message = ?1 AND NOT deleted AND NOT done")?;
                    stmt.query_map(params![id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
                };
                if on {
                    self.set_flagged(&[id], true)?;
                    if !open.is_empty() {
                        continue;
                    }
                    let title = if subject.trim().is_empty() { "(no subject)".to_string() } else { subject.clone() };
                    if outlook {
                        let mid: Option<String> = self.conn.query_row("SELECT message_id FROM messages WHERE id = ?1", params![id], |r| r.get(0))?;
                        self.conn.execute(
                            "INSERT INTO todos (title, notes, message, message_id, list_name, created, updated) VALUES (?1, '', ?2, ?3, 'Flagged email', ?4, ?4)",
                            params![title, id, mid, now()],
                        )?;
                        made.push(self.conn.last_insert_rowid());
                    } else {
                        made.push(self.todo_add(&title, "", None, Some(id))?);
                    }
                } else {
                    self.set_flagged(&[id], false)?;
                    for t in open {
                        let list: String = self.conn.query_row("SELECT list_name FROM todos WHERE id = ?1", params![t], |r| r.get(0))?;
                        if list == "Flagged email" {
                            // (Outlook takes its own task away with the flag)
                            self.conn.execute("UPDATE todos SET done = 1, done_at = ?2 WHERE id = ?1", params![t, now()])?;
                        } else {
                            self.todo_update(t, None, None, None, Some(true), None)?;
                        }
                    }
                }
            }
            Ok(())
        })?;
        Ok(made)
    }

    /// Sets or clears the To Do flag on messages (Outlook's flag goes with it).
    pub fn set_flagged(&self, ids: &[i64], on: bool) -> Result<u64> {
        self.change_flags(ids, |f| if on { f | crate::marks::FLAGGED } else { f & !crate::marks::FLAGGED })
    }

    /// The task changes waiting for Microsoft To Do (oldest first), with each task as Graph takes it.
    pub fn todo_jobs(&self, limit: usize) -> Result<Vec<TodoJob>> {
        let ops: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, payload FROM pending_ops WHERE kind = 'todo-change' AND attempts < 10 ORDER BY id LIMIT ?1")?;
            stmt.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let mut jobs = Vec::new();
        for (op, payload) in ops {
            let v: Value = serde_json::from_str(&payload)?;
            let (Some(todo), Some(action)) = (v["todo"].as_i64(), v["action"].as_str()) else {
                self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                continue;
            };
            let found: Option<(Option<String>, Option<String>, Option<i64>)> =
                self.conn.query_row("SELECT list_id, server_id, message FROM todos WHERE id = ?1", params![todo], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            let Some((list, server_id, message)) = found else {
                self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
                continue;
            };
            let subject: Option<String> = match message {
                Some(m) => self.conn.query_row("SELECT subject FROM messages WHERE id = ?1", params![m], |r| r.get(0)).optional()?,
                None => None,
            };
            let task = to_graph(&self.todo(todo)?, subject.as_deref());
            jobs.push(TodoJob { op, todo, action: action.to_string(), list, server_id, task });
        }
        Ok(jobs)
    }

    /// Microsoft To Do read back: tasks added, changed (by its last-modified time) or gone; a
    /// "Flagged email" task matched to its message (and a task standing in for it here); a
    /// task carrying a Message-ID matched to that message on this computer. Tasks with a change
    /// of their own waiting keep it.
    pub fn record_todos(&self, account: i64, lists: &[(Value, Vec<Value>)]) -> Result<TodoRecorded> {
        // To Do kept local (the calendar switch of the account it syncs through, `local_only`):
        // only tasks new here come in; those here keep their own version, none leaves
        let local = self.local_only_account(account, crate::local_only::Service::Calendar)?;
        let mut done = TodoRecorded::default();
        let waiting: HashSet<i64> = {
            let mut stmt = self.conn.prepare("SELECT json_extract(payload, '$.todo') FROM pending_ops WHERE kind = 'todo-change'")?;
            stmt.query_map([], |r| r.get::<_, Option<i64>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().flatten().collect()
        };
        let known: HashMap<String, (i64, Option<String>)> = {
            let mut stmt = self.conn.prepare("SELECT server_id, id, version FROM todos WHERE server_id IS NOT NULL")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?.collect::<rusqlite::Result<_>>()?
        };
        let mut seen = HashSet::new();
        self.in_transaction(|| {
            for (list, tasks) in lists {
                let list_id = list["id"].as_str().unwrap_or("");
                let flagged_list = list["wellknownListName"].as_str() == Some("flaggedEmails");
                let list_name = if flagged_list { "Flagged email".to_string() } else { list["displayName"].as_str().unwrap_or("Tasks").to_string() };
                for t in tasks {
                    let Some(sid) = t["id"].as_str() else { continue };
                    seen.insert(sid.to_string());
                    let version = t["lastModifiedDateTime"].as_str().map(str::to_string);
                    let title = t["title"].as_str().unwrap_or("").to_string();
                    let notes = t["body"]["content"].as_str().unwrap_or("").to_string();
                    let notes = if t["body"]["contentType"].as_str() == Some("html") { mail_parser::decoders::html::html_to_text(&notes).trim().to_string() } else { notes };
                    let is_done = t["status"].as_str() == Some("completed");
                    let important = t["importance"].as_str() == Some("high");
                    let due = day_of(&t["dueDateTime"]);
                    // the message it's about: Eudora's Message-ID, else Outlook's message id
                    let link = t["linkedResources"].as_array().and_then(|l| l.first()).cloned().unwrap_or(Value::Null);
                    // (a flagged email's Message-ID, asked of Microsoft by the read-back, first)
                    let external = link["internetMessageId"].as_str().or(link["externalId"].as_str()).unwrap_or("");
                    let message: Option<i64> = if external.starts_with('<') || external.contains('@') {
                        // (Eudora keeps a Message-ID without its angle brackets; Microsoft sends them)
                        let bare = external.trim().trim_start_matches('<').trim_end_matches('>');
                        self.conn.query_row("SELECT id FROM messages WHERE message_id IN (?1, ?2) LIMIT 1", params![bare, external], |r| r.get(0)).optional()?
                    } else if !external.is_empty() {
                        self.conn.query_row("SELECT message FROM server_items WHERE server_id = ?1 LIMIT 1", params![external], |r| r.get(0)).optional()?
                    } else {
                        None
                    };
                    let message_id: Option<String> = match message {
                        Some(m) => self.conn.query_row("SELECT message_id FROM messages WHERE id = ?1", params![m], |r| r.get(0)).optional()?,
                        None => (!external.is_empty() && external.contains('@')).then(|| external.to_string()),
                    };
                    match known.get(sid) {
                        Some(_) if local => {}
                        Some((id, _)) if waiting.contains(id) => {}
                        Some((id, v)) if *v == version => {
                            // unchanged there; its message, when found only now, is filled in
                            if message.is_some() {
                                self.conn.execute("UPDATE todos SET message = ?2, message_id = COALESCE(message_id, ?3) WHERE id = ?1 AND message IS NULL", params![id, message, message_id])?;
                            }
                        }
                        Some((id, _)) => {
                            self.conn.execute(
                                "UPDATE todos SET title = ?2, notes = ?3, due = ?4, done = ?5, important = ?6, done_at = CASE WHEN ?5 THEN COALESCE(done_at, ?7) ELSE NULL END,
                                 list_id = ?8, list_name = ?9, version = ?10, message = COALESCE(?11, message), message_id = COALESCE(?12, message_id), deleted = 0, updated = ?7 WHERE id = ?1",
                                params![id, title, notes, due, is_done, important, now(), list_id, list_name, version, message, message_id],
                            )?;
                            done.changed += 1;
                        }
                        None => {
                            // a flagged message's stand-in here becomes this task
                            let stand_in: Option<i64> = match (flagged_list, message) {
                                (true, Some(m)) => self.conn.query_row("SELECT id FROM todos WHERE message = ?1 AND server_id IS NULL AND list_name = 'Flagged email' LIMIT 1", params![m], |r| r.get(0)).optional()?,
                                _ => None,
                            };
                            match stand_in {
                                Some(id) => {
                                    self.conn.execute(
                                        "UPDATE todos SET server_id = ?2, list_id = ?3, version = ?4, title = ?5, due = ?6, done = ?7, important = ?8, updated = ?9 WHERE id = ?1",
                                        params![id, sid, list_id, version, title, due, is_done, important, now()],
                                    )?;
                                    done.changed += 1;
                                }
                                None => {
                                    self.conn.execute(
                                        "INSERT INTO todos (title, notes, due, done, important, done_at, message, message_id, list_id, list_name, server_id, version, created, updated)
                                         VALUES (?1, ?2, ?3, ?4, ?5, CASE WHEN ?4 THEN ?6 END, ?7, ?8, ?9, ?10, ?11, ?12, ?6, ?6)",
                                        params![title, notes, due, is_done, important, now(), message, message_id, list_id, list_name, sid, version],
                                    )?;
                                    done.added += 1;
                                }
                            }
                            // a message flagged in Outlook shows flagged here too
                            if flagged_list && !is_done {
                                if let Some(m) = message {
                                    self.conn.execute("UPDATE messages SET flags = flags | ?2 WHERE id = ?1", params![m, crate::marks::FLAGGED])?;
                                }
                            }
                        }
                    }
                    // unflagged in Outlook (its flagged-email task completed or gone): the flag goes here
                    if flagged_list && is_done {
                        if let Some(m) = message {
                            self.conn.execute("UPDATE messages SET flags = flags & ~?2 WHERE id = ?1", params![m, crate::marks::FLAGGED])?;
                        }
                    }
                }
            }
            // tasks gone from Microsoft To Do (deleted in Outlook or the app)
            for (sid, (id, _)) in &known {
                if !local && !seen.contains(sid) && !waiting.contains(id) {
                    let flagged: Option<i64> = self.conn.query_row("SELECT message FROM todos WHERE id = ?1 AND list_name = 'Flagged email'", params![id], |r| r.get(0)).optional()?.flatten();
                    if let Some(m) = flagged {
                        self.conn.execute("UPDATE messages SET flags = flags & ~?2 WHERE id = ?1", params![m, crate::marks::FLAGGED])?;
                    }
                    self.conn.execute("DELETE FROM todos WHERE id = ?1", params![id])?;
                    done.removed += 1;
                }
            }
            Ok(())
        })?;
        // the lists themselves, for showing or hiding each (whose they are: Microsoft To Do says
        // whether the user owns a list or it was shared with them)
        let known: Vec<TodoList> = lists
            .iter()
            .filter_map(|(l, _)| {
                let id = l["id"].as_str()?.to_string();
                let flagged = l["wellknownListName"].as_str() == Some("flaggedEmails");
                Some(TodoList {
                    name: if flagged { "Flagged email".into() } else { l["displayName"].as_str().unwrap_or("Tasks").to_string() },
                    owner: l["isOwner"].as_bool().unwrap_or(true),
                    shared: l["isShared"].as_bool().unwrap_or(false),
                    default: l["wellknownListName"].as_str() == Some("defaultList"),
                    flagged,
                    shown: true,
                    open: 0,
                    color: String::new(),
                    own_color: false,
                    id,
                })
            })
            .collect();
        self.set_setting(LISTS_SETTING, &serde_json::to_string(&known)?)?;
        Ok(done)
    }

    /// The To Do lists, each with whether it's shown and how many of its tasks are open. Lists the
    /// user owns are shown until hidden; lists someone else shared start hidden.
    pub fn todo_lists(&self) -> Result<Vec<TodoList>> {
        let mut lists: Vec<TodoList> = self.setting(LISTS_SETTING)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let shown = self.todo_shown()?;
        let colors: HashMap<String, String> = self.setting(COLORS_SETTING)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let mut stmt = self.conn.prepare("SELECT list_id, COUNT(*) FROM todos WHERE NOT deleted AND NOT done AND list_id IS NOT NULL GROUP BY list_id")?;
        let open: HashMap<String, i64> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for l in &mut lists {
            l.shown = *shown.get(&l.id).unwrap_or(&l.owner);
            l.open = *open.get(&l.id).unwrap_or(&0) as u64;
            l.own_color = colors.contains_key(&l.id);
            l.color = colors.get(&l.id).cloned().unwrap_or_else(|| first_color(l));
        }
        // (the default list first, then flagged email, then the user's own, then others', by name)
        lists.sort_by(|a, b| (!a.default, !a.flagged, !a.owner, a.name.to_lowercase()).cmp(&(!b.default, !b.flagged, !b.owner, b.name.to_lowercase())));
        Ok(lists)
    }

    /// Shows or hides a To Do list (only here: the list and its tasks stay in Microsoft To Do).
    pub fn set_todo_list_shown(&self, id: &str, shown: bool) -> Result<()> {
        let mut map = self.todo_shown()?;
        map.insert(id.to_string(), shown);
        self.set_setting(SHOWN_SETTING, &serde_json::to_string(&map)?)
    }

    /// A list's color in Eudora ("#rrggbb"), or None for its first color again.
    pub fn set_todo_list_color(&self, id: &str, color: Option<&str>) -> Result<()> {
        let mut colors: HashMap<String, String> = self.setting(COLORS_SETTING)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        match color {
            Some(c) => {
                if !(c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit())) {
                    return Err(Error::Other(format!("“{c}” isn't a color: #rrggbb")));
                }
                colors.insert(id.to_string(), c.to_ascii_lowercase());
            }
            None => {
                colors.remove(id);
            }
        }
        self.set_setting(COLORS_SETTING, &serde_json::to_string(&colors)?)
    }

    fn todo_shown(&self) -> Result<HashMap<String, bool>> {
        Ok(self.setting(SHOWN_SETTING)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    /// The lists hidden now (the ones chosen, and others' shared lists not chosen).
    fn hidden_lists(&self) -> Result<HashSet<String>> {
        Ok(self.todo_lists()?.into_iter().filter(|l| !l.shown).map(|l| l.id).collect())
    }

    /// A change made on Microsoft To Do: the task's server id and version recorded, or (deleted)
    /// the task gone here too.
    pub fn record_todo_done(&self, op: i64, todo: i64, action: &str, task: Option<&Value>) -> Result<()> {
        if action == "delete" {
            self.conn.execute("DELETE FROM todos WHERE id = ?1", params![todo])?;
        } else if let Some(v) = task {
            self.conn.execute(
                "UPDATE todos SET server_id = COALESCE(?2, server_id), version = ?3, list_id = COALESCE(?4, list_id) WHERE id = ?1",
                params![todo, v["id"].as_str(), v["lastModifiedDateTime"].as_str(), v["parentListId"].as_str()],
            )?;
        }
        self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
        Ok(())
    }

    // ---------------------------------------------------------------- Twos

    /// Twos' user id and whether a token is kept (the token itself never leaves the credential store).
    pub fn twos_settings(&self) -> Result<(Option<String>, bool)> {
        let user = self.setting("twos-user")?;
        let token = user.as_deref().is_some_and(|u| crate::graph::load_secret(&format!("twos:{u}")).is_some());
        Ok((user, token))
    }

    /// Keeps Twos' user id (in the library) and token (in this computer's credential store).
    pub fn set_twos(&self, user: &str, token: Option<&str>) -> Result<()> {
        let user = user.trim();
        self.set_setting("twos-user", user)?;
        if let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) {
            crate::graph::store_secret(&format!("twos:{user}"), t.as_bytes())?;
        }
        Ok(())
    }

    /// Sends a task to Twos (its "today" list), with its due date, notes and the message it's about.
    pub fn send_todo_to_twos(&self, id: i64) -> Result<()> {
        let t = self.todo(id)?;
        let mut text = t.title.clone();
        if let Some(d) = t.due {
            text.push_str(&format!(" (due {})", chrono::DateTime::from_timestamp(d, 0).unwrap_or_default().format("%a %b %-d")));
        }
        if !t.notes.trim().is_empty() {
            text.push_str(&format!(" — {}", t.notes.trim()));
        }
        self.twos_add(&text)
    }

    /// Adds a line to Twos' "today" list. Twos has no documented API: this is the address its
    /// own apps use.
    pub fn twos_add(&self, text: &str) -> Result<()> {
        let (Some(user), true) = self.twos_settings()? else {
            return Err(Error::Other("Twos isn't set up: Settings ▸ To Do ▸ Twos (your user id and token)".into()));
        };
        let token = crate::graph::load_secret(&format!("twos:{user}")).and_then(|b| String::from_utf8(b).ok()).ok_or_else(|| Error::Other("Twos' token isn't on this computer".into()))?;
        crate::twos::add_to_today(&user, &token, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library() -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::Silent).unwrap();
        (dir, library)
    }

    /// Lists someone else shared start hidden; any list can be shown or hidden, and its tasks
    /// leave or join To Do (and its count) with it.
    #[test]
    fn shared_lists_start_hidden_and_any_list_can_be_shown_or_hidden() {
        let (_d, l) = library();
        let task = |id: &str, title: &str| json!({ "id": id, "title": title, "status": "notStarted", "lastModifiedDateTime": "2026-11-09T12:00:00Z" });
        let mine = json!({ "id": "L1", "displayName": "Tasks", "wellknownListName": "defaultList", "isOwner": true, "isShared": false });
        let theirs = json!({ "id": "L2", "displayName": "Sam's To-Do", "wellknownListName": "none", "isOwner": false, "isShared": true });
        l.record_todos(1, &[(mine, vec![task("T1", "Mine")]), (theirs, vec![task("T2", "Sam's"), task("T3", "Sam's too")])]).unwrap();
        let titles = |l: &Library| l.todos().unwrap().into_iter().map(|t| t.title).collect::<Vec<_>>();
        assert_eq!(titles(&l), vec!["Mine"]);
        assert_eq!(l.todos_open().unwrap(), 1);
        let lists = l.todo_lists().unwrap();
        assert_eq!(lists.iter().map(|x| (x.name.as_str(), x.shown, x.open)).collect::<Vec<_>>(), vec![("Tasks", true, 1), ("Sam's To-Do", false, 2)]);
        l.set_todo_list_shown("L2", true).unwrap();
        assert_eq!(titles(&l).len(), 3);
        l.set_todo_list_shown("L1", false).unwrap();
        assert_eq!(l.todos_open().unwrap(), 2);
        // colors: the default list's first, then the user's, then back
        assert_eq!(l.todo_lists().unwrap()[0].color, "#0078d4");
        l.set_todo_list_color("L1", Some("#2E9E4F")).unwrap();
        assert_eq!((l.todo_lists().unwrap()[0].color.as_str(), l.todo_lists().unwrap()[0].own_color), ("#2e9e4f", true));
        assert!(l.set_todo_list_color("L1", Some("green")).is_err());
        l.set_todo_list_color("L1", None).unwrap();
        assert_eq!(l.todo_lists().unwrap()[0].color, "#0078d4");
    }

    /// A flagged email's task finds its message by the Message-ID the read-back adds (Outlook's
    /// own id changes when the message moves), even when the task itself didn't change.
    #[test]
    fn a_flagged_email_task_finds_its_message_by_message_id() {
        let (_d, l) = library();
        l.conn
            .execute(
                "INSERT INTO messages (id, content_hash, message_id, date, date_source, from_display, subject, snippet, size, attachment_count, parse_status, pack, pack_offset, pack_len, added)
                 VALUES (77, x'01', 'abc@x.com', 0, 0, '', 'Budget', '', 10, 0, 0, 0, 0, 10, 0)",
                [],
            )
            .unwrap();
        let list = json!({ "id": "F", "displayName": "Flagged Emails", "wellknownListName": "flaggedEmails", "isOwner": true });
        let task = |with_mid: bool| {
            let mut link = json!({ "externalId": "AAMk-old-id" });
            if with_mid {
                link["internetMessageId"] = "<abc@x.com>".into();
            }
            json!({ "id": "T9", "title": "Budget", "status": "notStarted", "lastModifiedDateTime": "2026-11-09T12:00:00Z", "linkedResources": [link] })
        };
        l.record_todos(1, &[(list.clone(), vec![task(false)])]).unwrap();
        assert_eq!(l.todos().unwrap()[0].message, None, "Outlook's old id leads nowhere");
        l.record_todos(1, &[(list, vec![task(true)])]).unwrap();
        assert_eq!(l.todos().unwrap()[0].message, Some(77), "found by its Message-ID, though the task didn't change");
    }

    #[test]
    fn tasks_are_made_changed_queued_and_read_back() {
        let (_d, l) = library();
        let id = l.todo_add("Call the bank", "about the wire", Some(1_800_000_000), None).unwrap();
        l.todo_update(id, None, None, None, None, Some(true)).unwrap();
        let jobs = l.todo_jobs(10).unwrap();
        assert_eq!(jobs.iter().map(|j| j.action.as_str()).collect::<Vec<_>>(), vec!["create"], "a change before it's made goes with its creation");
        assert_eq!(jobs[0].task["importance"], "high");
        l.record_todo_done(jobs[0].op, id, "create", Some(&json!({ "id": "T1", "lastModifiedDateTime": "2026-10-19T12:00:00Z", "parentListId": "L1" }))).unwrap();
        assert!(l.todo(id).unwrap().synced);
        // read back: changed in the To Do app, a new one there, then gone
        let list = json!({ "id": "L1", "displayName": "Tasks", "wellknownListName": "defaultList" });
        let changed = json!({ "id": "T1", "title": "Call the bank today", "status": "completed", "importance": "normal", "lastModifiedDateTime": "2026-10-19T13:00:00Z", "body": { "content": "", "contentType": "text" } });
        let fresh = json!({ "id": "T2", "title": "Buy milk", "status": "notStarted", "lastModifiedDateTime": "2026-10-19T13:00:00Z", "dueDateTime": { "dateTime": "2026-10-21T00:00:00.0000000", "timeZone": "UTC" } });
        let r = l.record_todos(1, &[(list.clone(), vec![changed, fresh])]).unwrap();
        assert_eq!(r, TodoRecorded { added: 1, changed: 1, removed: 0 });
        let all = l.todos().unwrap();
        assert!(all.iter().any(|t| t.title == "Call the bank today" && t.done));
        assert!(all.iter().any(|t| t.title == "Buy milk" && t.due == Some(1_792_540_800)));
        assert_eq!(l.record_todos(1, &[(list, vec![])]).unwrap().removed, 2);
        assert!(l.todos().unwrap().is_empty());
    }

    #[test]
    fn deleting_a_task_not_made_yet_just_drops_it() {
        let (_d, l) = library();
        let id = l.todo_add("Draft", "", None, None).unwrap();
        l.todo_delete(id).unwrap();
        assert!(l.todo_jobs(10).unwrap().is_empty());
        assert!(l.todos().unwrap().is_empty());
    }
}
