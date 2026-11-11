//! Before older mail leaves this computer, Eudora asks each server whether it still has it.
//!
//! Every message Eudora downloaded records where it was on its server (`server_items`: the
//! folder and the server's id for it). For each server folder holding messages about to go, the
//! server is asked for the ids it has now: one listing per folder, not a request per message
//! (Microsoft 365 and shared mailboxes 1,000 ids a request, the Online Archive's folders through
//! the archive's own listing, an IMAP folder in one UID SEARCH, a Gmail label 500 a request). A
//! message is confirmed when at least one of its places is still there. Anything else stays:
//! a folder the server didn't answer for, an account not signed in on this computer, POP3 mail
//! (POP3 servers keep mail only until it's collected), imported files, classic Eudora
//! mailboxes. Nothing is ever removed on a guess.

use std::collections::{HashMap, HashSet};

use rusqlite::params;

use crate::graph::GraphClient;
use crate::library::{Library, in_online_archive};
use crate::progress::{Progress, thousands};
use crate::{Error, Result};

/// What the servers said.
#[derive(Debug, Default)]
pub struct Confirmed {
    /// Messages at least one server still has.
    pub on_server: HashSet<i64>,
    /// What couldn't be asked (each folder or account once), for the log.
    pub notes: Vec<String>,
}

/// One server folder and the messages Eudora has from it.
struct Group {
    kind: String,
    address: String,
    path: String,
    folder: String,
    items: Vec<(i64, String)>,
}

/// Asks the servers which of these messages they still have. `clients`: the Microsoft sign-ins
/// on this computer (the first also opens the shared mailboxes); IMAP and Gmail accounts are
/// signed in here with what this computer keeps for them.
pub fn confirm_on_servers(library: &Library, clients: &mut [GraphClient], ids: &[i64], progress: &dyn Progress) -> Result<Confirmed> {
    let mut out = Confirmed::default();
    if ids.is_empty() {
        return Ok(out);
    }
    let groups = places(library, ids)?;
    let total = groups.len();
    // the Online Archive: each folder's real mailbox (an auto-expanding archive spans several),
    // read once per Microsoft account that has archive folders here
    let mut archive_owners: HashMap<String, HashMap<String, String>> = HashMap::new();
    let imap = library.imap_accounts()?;
    for (n, g) in groups.iter().enumerate() {
        if progress.canceled() {
            return Err(Error::Canceled);
        }
        progress.report(n as u64, total as u64, &format!(
            "Checking the servers still have the older mail: folder {} of {} ({}: {}, {} messages)",
            n + 1, total, g.address, g.path, thousands(g.items.len() as u64)
        ));
        let present: Result<HashSet<String>> = match g.kind.as_str() {
            "graph" | "graph-shared" => {
                let shared = g.kind == "graph-shared";
                let found = if shared { clients.first_mut() } else { clients.iter_mut().find(|c| c.account.eq_ignore_ascii_case(&g.address)) };
                match found {
                    None => Err(Error::Other(format!("{} isn't signed in on this computer", if shared { "Microsoft 365" } else { g.address.as_str() }))),
                    Some(client) if in_online_archive(&g.path) => archive_ids(client, &mut archive_owners, &g.folder),
                    Some(client) => {
                        if shared {
                            client.set_mailbox(Some(g.address.clone()));
                        }
                        let listed = client.message_ids(&g.folder, &|_| {}).map(|ids| ids.into_iter().map(|(id, _)| id).collect());
                        if shared {
                            client.set_mailbox(None);
                        }
                        listed
                    }
                }
            }
            "imap" => match imap.iter().find(|c| c.address.eq_ignore_ascii_case(&g.address) && !c.is_pop3()) {
                None => Err(Error::Other(format!("{}'s settings aren't on this computer", g.address))),
                Some(config) => crate::imap_sync::Connection::open(config, None).and_then(|mut connection| {
                    let listed = connection.list(&g.folder).map(|items| items.into_iter().map(|i| i.id).collect());
                    connection.logout();
                    listed
                }),
            },
            "gmail" => crate::google::GoogleClient::resume(&g.address)
                .and_then(|mut client| client.message_ids(if g.folder == "ALL" { "" } else { &g.folder }, &|_| {}).map(|ids| ids.into_iter().collect())),
            other => Err(Error::Other(format!("{other} accounts keep no mail on a server"))),
        };
        match present {
            Ok(present) => {
                for (message, server_id) in &g.items {
                    if present.contains(server_id) {
                        out.on_server.insert(*message);
                    }
                }
            }
            Err(Error::Canceled) => return Err(Error::Canceled),
            Err(e) => out.notes.push(format!("{}: {} couldn't be checked ({e}); its {} older messages stay", g.address, g.path, thousands(g.items.len() as u64))),
        }
    }
    Ok(out)
}

/// The ids in an Online Archive folder (the mailbox APIs).
fn archive_ids(client: &mut GraphClient, owners: &mut HashMap<String, HashMap<String, String>>, folder: &str) -> Result<HashSet<String>> {
    if !owners.contains_key(&client.account) {
        let (_, archive) = client.mailbox_ids()?;
        let archive = archive.ok_or_else(|| Error::Other("the Online Archive can't be read".into()))?;
        let map = client.mailbox_folders(&archive, &|_| {})?.iter().map(|f| (f.id.clone(), GraphClient::folder_owner(&archive, f))).collect();
        owners.insert(client.account.clone(), map);
    }
    let owner = owners[&client.account].get(folder).cloned().ok_or_else(|| Error::Other("the folder is no longer in the Online Archive".into()))?;
    Ok(client.all_folder_items(&owner, folder, &|_| {})?.into_iter().map(|i| i.id).collect())
}

/// Where on their servers these messages were: one group per server folder.
fn places(library: &Library, ids: &[i64]) -> Result<Vec<Group>> {
    let conn = &library.conn;
    conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS keep_check (message INTEGER PRIMARY KEY); DELETE FROM keep_check;")?;
    library.in_transaction(|| {
        let mut insert = conn.prepare_cached("INSERT OR IGNORE INTO keep_check (message) VALUES (?1)")?;
        for id in ids {
            insert.execute(params![id])?;
        }
        Ok(())
    })?;
    let mut stmt = conn.prepare(
        "SELECT s.message, s.server_id, a.kind, a.address, f.path, f.server_id FROM keep_check k
           JOIN server_items s ON s.message = k.message JOIN folders f ON f.id = s.folder JOIN accounts a ON a.id = f.account
          WHERE f.server_id IS NOT NULL AND f.server_id <> '' ORDER BY f.id",
    )?;
    let mut groups: Vec<Group> = Vec::new();
    let mut at: HashMap<(String, String, String), usize> = HashMap::new();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?)))?;
    for row in rows {
        let (message, server_id, kind, address, path, folder) = row?;
        let key = (kind.clone(), address.to_lowercase(), folder.clone());
        let i = *at.entry(key).or_insert_with(|| {
            groups.push(Group { kind, address, path, folder, items: Vec::new() });
            groups.len() - 1
        });
        groups[i].items.push((message, server_id));
    }
    drop(stmt);
    conn.execute("DELETE FROM keep_check", [])?;
    Ok(groups)
}
