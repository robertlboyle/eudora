//! Running filing sync: for each account this computer has, read the envelopes every computer
//! stored for it (sealed with the user's sync key), merge them with this computer's new filing,
//! store this computer's copy when something changed, and file the messages here as the merged
//! log says (the project's design notes § 7).
//!
//! Where an account's envelopes live: a Microsoft 365 account's (and a shared mailbox's) in that mailbox's hidden
//! "Eudora Sync" folder; a Gmail account's under a hidden "Eudora Sync" label, an IMAP account's in an "Eudora
//! Sync" folder on its server (2026-10-26, `sync_stores`); an account with no server sync can use here (a classic
//! Eudora archive, imported files, POP3) in the **home account**'s, which can be any of those kinds (Microsoft
//! 365 first, else Gmail, else IMAP). Each computer writes its own envelope (named by key and device) and readers
//! merge them all, so two computers saving at once never overwrite each other.

use crate::error::{Error, Result};
use crate::filing::Applied;
use crate::filing_sync::Log;
use crate::graph::{GraphClient, SYNC_SUBJECT};
use crate::library::{Library, now};
use crate::sync_envelope::SyncKey;

/// Changes older than this join the snapshot.
pub const FOLD_AFTER: i64 = 30 * 86_400;
/// Setting: the home account's address (default: `default_home`). This
/// computer's own choice (not synced).
pub const HOME_SETTING: &str = "sync-home";
/// Setting: other accounts that also carry the settings (JSON list of addresses).
pub const SETTINGS_ACCOUNTS: &str = "sync-settings-accounts";

/// Where envelopes are kept (a mailbox's sync folder; a test's memory).
pub trait SyncStore {
    /// Every envelope there: (its id in the store, its name, its bytes).
    fn list(&mut self) -> Result<Vec<(String, String, Vec<u8>)>>;
    fn put(&mut self, name: &str, bytes: &[u8]) -> Result<String>;
    fn delete(&mut self, id: &str) -> Result<()>;
    /// This computer may store there (Microsoft 365: a sign-in that may change mail).
    fn writable(&self) -> bool;
    /// The most one item may carry: a bigger envelope is stored in parts.
    fn max_item(&self) -> usize;
}

/// A mailbox's "Eudora Sync" folder (the user's own, or a shared mailbox's).
pub struct GraphStore<'a> {
    client: &'a mut GraphClient,
    mailbox: Option<String>,
    folder: Option<String>,
    write: bool,
}

impl<'a> GraphStore<'a> {
    pub fn open(client: &'a mut GraphClient, mailbox: Option<String>) -> Result<GraphStore<'a>> {
        let write = client.may_change_mail();
        client.set_mailbox(mailbox.clone());
        let folder = client.sync_folder(write);
        client.set_mailbox(None);
        Ok(GraphStore { client, mailbox, folder: folder?, write })
    }
}

impl SyncStore for GraphStore<'_> {
    fn list(&mut self) -> Result<Vec<(String, String, Vec<u8>)>> {
        let Some(folder) = self.folder.clone() else { return Ok(Vec::new()) };
        self.client.set_mailbox(self.mailbox.clone());
        let items = self.client.sync_items(&folder);
        self.client.set_mailbox(None);
        items
    }

    fn put(&mut self, name: &str, bytes: &[u8]) -> Result<String> {
        let folder = self.folder.clone().ok_or_else(|| Error::Other("no sync folder".into()))?;
        self.client.set_mailbox(self.mailbox.clone());
        let id = self.client.put_sync_item(&folder, name, bytes);
        self.client.set_mailbox(None);
        id
    }

    fn delete(&mut self, id: &str) -> Result<()> {
        self.client.set_mailbox(self.mailbox.clone());
        let done = self.client.delete_message(id);
        self.client.set_mailbox(None);
        done
    }

    fn writable(&self) -> bool {
        self.write && self.folder.is_some()
    }

    fn max_item(&self) -> usize {
        crate::graph::SYNC_ITEM_LIMIT
    }
}

/// What syncing one account's filing did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FilingSynced {
    /// Envelopes read (one per computer that stored one).
    pub computers: usize,
    /// This computer's filing added to the log.
    pub sent: usize,
    /// This computer's copy stored again.
    pub stored: bool,
    /// In how many parts (items) it was stored.
    pub parts: usize,
    /// Computers whose newest upload here is damaged or cut short (their changes may be lost).
    pub broken: Vec<String>,
    /// This computer asked for a full copy (an SOS: bad data found, or `eudora-cli sync repair`).
    pub asked: bool,
    /// SOS requests from other computers this one answered with a full copy.
    pub answered: usize,
    pub applied: Applied,
}

/// An envelope part's name: "Eudora sync · <kind> · <scope> · <key id> · <device> · <upload> ·
/// <part>/<parts>": kind "filing" (scope: the account) or "settings" (scope "all"). `upload`
/// (when it was stored, 12 hex digits of milliseconds) tells one upload's parts from another's
/// and which is newest.
fn part_name(kind: &str, scope: &str, key: &SyncKey, device: &str, upload: &str, part: usize, parts: usize) -> String {
    format!("{SYNC_SUBJECT} · {kind} · {scope} · {} · {device} · {upload} · {part}/{parts}", key.id())
}

/// What a part's name says.
#[derive(Debug, PartialEq)]
struct PartName {
    kind: String,
    scope: String,
    key: String,
    device: String,
    upload: String,
    part: usize,
    parts: usize,
}

fn parse_name(name: &str) -> Option<PartName> {
    let fields: Vec<&str> = name.split(" · ").collect();
    let named = |kind: &str, scope: &str, key: &str, device: &str, upload: &str, part, parts| PartName {
        kind: kind.to_string(),
        scope: scope.to_lowercase(),
        key: key.to_string(),
        device: device.to_string(),
        upload: upload.to_string(),
        part,
        parts,
    };
    match fields.as_slice() {
        // one envelope in one message (the first format)
        [_, kind @ ("filing" | "settings"), scope, key, device] => Some(named(kind, scope, key, device, "", 1, 1)),
        [_, kind @ ("filing" | "settings"), scope, key, device, upload, of] => {
            let (part, parts) = of.split_once('/')?;
            let (part, parts): (usize, usize) = (part.parse().ok()?, parts.parse().ok()?);
            (1..=parts).contains(&part).then(|| named(kind, scope, key, device, upload, part, parts))
        }
        _ => None,
    }
}

/// An SOS's name: "Eudora sync · sos · <kind>:<scope> · <key id> · <device> · <when>": that
/// computer found bad data and asks one with a complete copy to store it again in full.
fn sos_name(topic: &str, key: &SyncKey, device: &str, upload: &str) -> String {
    format!("{SYNC_SUBJECT} · sos · {topic} · {} · {device} · {upload}", key.id())
}

fn parse_sos(name: &str) -> Option<(String, String, String)> {
    let fields: Vec<&str> = name.split(" · ").collect();
    match fields.as_slice() {
        [_, "sos", topic, key, device, _] => {
            // (the first SOS format named the account alone: filing)
            let topic = if topic.contains(':') { topic.to_lowercase() } else { format!("filing:{}", topic.to_lowercase()) };
            Some((topic, key.to_string(), device.to_string()))
        }
        _ => None,
    }
}

/// Now, as an upload's name (12 hex digits of milliseconds: they sort by time).
fn upload_stamp() -> String {
    format!("{:012x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()))
}

/// How long ago an upload was stored (by its name; the first format's have none: long ago).
fn upload_age(upload: &str) -> std::time::Duration {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let then = u128::from_str_radix(upload, 16).unwrap_or(0);
    std::time::Duration::from_millis(now.saturating_sub(then).min(u128::from(u64::MAX)) as u64)
}

/// An upload with parts missing is taken for one in progress this long, then as cut short.
const UPLOAD_GRACE: std::time::Duration = std::time::Duration::from_secs(600);

/// Where this computer keeps its own copy of a merged log or settings (`<library>/sync/`).
fn cache_path(library: &Library, name: &str) -> std::path::PathBuf {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "@.-_".contains(c) { c } else { '_' }).collect();
    library.root.join("sync").join(safe)
}

fn read_cache(library: &Library, address: &str) -> Option<Log> {
    std::fs::read(cache_path(library, &format!("filing-{address}.log"))).ok().and_then(|b| Log::decode(&b).ok()).filter(|l| l.account == address)
}

/// Keeps a copy (written whole, then renamed: never half a file).
fn write_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use crate::error::IoContext;
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).context(|| format!("making {}", dir.display()))?;
    let part = path.with_extension("part");
    std::fs::write(&part, bytes).context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, path).context(|| format!("writing {}", path.display()))
}

fn write_cache(library: &Library, log: &Log) -> Result<()> {
    write_file(&cache_path(library, &format!("filing-{}.log", log.account)), &log.encode())
}

/// One upload, as found: its parts by number.
#[derive(Default)]
struct Upload {
    parts: usize,
    bytes: std::collections::BTreeMap<usize, Vec<u8>>,
    /// Its items in the store.
    ids: Vec<String>,
}

impl Upload {
    /// The envelope, when every part is there (a cut-short upload isn't used).
    fn whole(&self) -> Option<Vec<u8>> {
        (self.parts > 0 && self.bytes.len() == self.parts && self.bytes.keys().copied().eq(1..=self.parts)).then(|| self.bytes.values().flatten().copied().collect())
    }
}

/// What a store holds for one topic ("filing:<account>", "settings:all"), sealed with our key.
#[derive(Default)]
struct Found {
    /// Each computer's newest usable upload, opened: (computer, its contents).
    copies: Vec<(String, Vec<u8>)>,
    /// This computer's items (replaced when it stores again).
    my_items: Vec<String>,
    /// Other computers whose newest upload is damaged or cut short.
    broken: Vec<String>,
    /// Their unusable items (removed when this computer answers an SOS).
    broken_items: Vec<String>,
    /// This computer's own newest upload is unusable.
    mine_broken: bool,
    /// SOS requests for the topic: (item, computer asking).
    sos: Vec<(String, String)>,
}

/// Reads a topic's uploads: each computer's newest whole one that opens (a newer one that's
/// damaged, or cut short past the grace, is noted: changes may be missing).
fn read_topic(store: &mut dyn SyncStore, kind: &str, scope: &str, key: &SyncKey, device: &str, check: &dyn Fn(&[u8]) -> bool) -> Result<Found> {
    let topic = format!("{kind}:{scope}");
    let mut found = Found::default();
    let mut uploads: std::collections::BTreeMap<String, std::collections::BTreeMap<String, Upload>> = Default::default();
    for (id, name, bytes) in store.list()? {
        if let Some((t, key_id, from)) = parse_sos(&name) {
            // only an SOS sealed with this key counts (nobody else can ask for our data)
            if t == topic && key_id == key.id() && key.open(&format!("sos:{topic}"), &bytes).is_ok() {
                found.sos.push((id, from));
            }
            continue;
        }
        let Some(n) = parse_name(&name) else { continue };
        if n.kind != kind || n.scope != scope || n.key != key.id() {
            continue; // another topic's, or another person's (their own key)
        }
        if n.device == device {
            found.my_items.push(id.clone());
        }
        let upload = uploads.entry(n.device).or_default().entry(n.upload).or_default();
        upload.parts = n.parts;
        upload.bytes.insert(n.part, bytes);
        upload.ids.push(id);
    }
    let purpose = if kind == "filing" { format!("filing:{scope}") } else { kind.to_string() };
    for (from, by_upload) in &uploads {
        let mut used = None;
        let mut broken = false;
        for (stamp, upload) in by_upload.iter().rev() {
            let unusable = match upload.whole() {
                None => upload_age(stamp) > UPLOAD_GRACE,
                Some(bytes) => match key.open(&purpose, &bytes).ok().filter(|plain| check(plain)) {
                    Some(plain) => {
                        used = Some(plain);
                        break;
                    }
                    None => true,
                },
            };
            if unusable {
                broken = true;
                if from != device {
                    found.broken_items.extend(upload.ids.iter().cloned());
                }
            }
        }
        if broken {
            if from == device {
                found.mine_broken = true;
            } else {
                found.broken.push(from.clone());
            }
        }
        if let Some(plain) = used {
            found.copies.push((from.clone(), plain));
        }
    }
    Ok(found)
}

/// Stores this computer's copy of a topic (in parts when it's bigger than one item carries;
/// every part before the old ones go, so a cut-short upload leaves the last whole one in use).
/// Returns how many parts.
fn store_topic(store: &mut dyn SyncStore, kind: &str, scope: &str, key: &SyncKey, device: &str, plain: &[u8], my_items: &[String]) -> Result<usize> {
    let purpose = if kind == "filing" { format!("filing:{scope}") } else { kind.to_string() };
    let sealed = key.seal(&purpose, plain)?;
    let size = store.max_item().max(1);
    let chunks: Vec<&[u8]> = sealed.chunks(size).collect();
    let upload = upload_stamp();
    for (i, chunk) in chunks.iter().enumerate() {
        store.put(&part_name(kind, scope, key, device, &upload, i + 1, chunks.len()), chunk)?;
    }
    for id in my_items {
        store.delete(id)?;
    }
    Ok(chunks.len())
}

/// After reading and (maybe) storing: answers SOS requests when this computer stored a full
/// copy, raises its own when something is unreadable (or asked by hand), lifts it once all
/// reads cleanly. Returns (asked, answered).
fn settle_sos(store: &mut dyn SyncStore, topic: &str, key: &SyncKey, device: &str, found: &Found, answered_now: bool, ask_for_full: bool) -> Result<(bool, usize)> {
    let mut answered = 0;
    let mut bad = !found.broken.is_empty();
    if answered_now {
        for id in &found.broken_items {
            store.delete(id)?;
        }
        for (id, from) in &found.sos {
            if from != device {
                store.delete(id)?;
                answered += 1;
            }
        }
        bad = false; // what was unreadable is gone, superseded by the copy just stored
    }
    let my_sos: Vec<&String> = found.sos.iter().filter(|(_, from)| from == device).map(|(id, _)| id).collect();
    let mut asked = false;
    if (bad || ask_for_full) && my_sos.is_empty() {
        store.put(&sos_name(topic, key, device, &upload_stamp()), &key.seal(&format!("sos:{topic}"), found.broken.join(",").as_bytes())?)?;
        asked = true;
    } else if !bad && !ask_for_full {
        for id in my_sos {
            store.delete(id)?; // everything reads cleanly now
        }
    }
    Ok((asked, answered))
}

/// Syncs one account's filing through `store`.
pub fn sync_account(library: &Library, store: &mut dyn SyncStore, key: &SyncKey, device: &str, account: i64, address: &str) -> Result<FilingSynced> {
    sync_account_with(library, store, key, device, account, address, false)
}

/// `sync_account`, and `ask_for_full`: send an SOS even when nothing looks wrong (`eudora-cli sync
/// repair`: this computer's filing doesn't look right).
///
/// The SOS: a computer that finds another's newest upload damaged or cut short stores an SOS;
/// a computer with a complete copy of its own answers by storing its whole merged log again
/// and removing what it supersedes and the SOS; the asker removes its own once it reads
/// everything cleanly. Applying here always uses the whole log, so the receiving side needs
/// nothing more.
pub fn sync_account_with(library: &Library, store: &mut dyn SyncStore, key: &SyncKey, device: &str, account: i64, address: &str, ask_for_full: bool) -> Result<FilingSynced> {
    let address = address.to_lowercase();
    let mut done = FilingSynced::default();
    // this computer's own complete copy (the last merged log), so it can answer an SOS even
    // when every copy in the mailbox is damaged
    let cached = read_cache(library, &address);
    let mut log = cached.clone().unwrap_or_else(|| Log::new(&address));
    let found = read_topic(store, "filing", &address, key, device, &|plain| Log::decode(plain).is_ok())?;
    let mut my_log: Option<Log> = None;
    for (from, plain) in &found.copies {
        let Ok(other) = Log::decode(plain) else { continue };
        log.merge(&other);
        done.computers += 1;
        if *from == device {
            my_log = Some(other);
        }
    }
    done.broken = found.broken.clone();
    // an SOS from another computer, and this one has a complete copy of its own: it answers
    // (even when some upload is unreadable: its copy supersedes those, and they go)
    let answer = (cached.is_some() || done.computers > 0) && found.sos.iter().any(|(_, from)| from != device);
    let mut taken = Vec::new();
    if store.writable() {
        taken = library.take_filing_pending(account, &mut log, device)?;
        done.sent = taken.len();
    }
    log.fold(now() - FOLD_AFTER);
    // stored only when this computer has filing of its own to add (or a full copy is needed):
    // every change lives in the copy of the computer that made it, and readers merge every
    // copy (a computer that only reads stores nothing, so space doesn't grow with computers)
    let store_now = store.writable() && ((!taken.is_empty() && my_log.as_ref() != Some(&log)) || answer || found.mine_broken);
    if store_now {
        done.parts = store_topic(store, "filing", &address, key, device, &log.encode(), &found.my_items)?;
        done.stored = true;
    }
    if store.writable() {
        (done.asked, done.answered) = settle_sos(store, &format!("filing:{address}"), key, device, &found, store_now && answer, ask_for_full)?;
    }
    write_cache(library, &log)?;
    if !taken.is_empty() {
        library.clear_filing_pending(&taken)?;
    }
    done.applied = library.apply_filing(&log)?;
    Ok(done)
}

/// What syncing the settings through one account did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsSynced {
    pub computers: usize,
    pub stored: bool,
    pub parts: usize,
    /// Settings changed here (newer on another computer).
    pub changed: usize,
    pub broken: Vec<String>,
    pub asked: bool,
    pub answered: usize,
}

/// Syncs the settings that travel (`settings_sync::SYNCED`) through `store`: every computer's
/// copy merged item by item with this computer's (the newest change of each wins), this
/// computer's copy stored when it changed something itself, the merged ones taken in here.
pub fn sync_settings(library: &Library, store: &mut dyn SyncStore, key: &SyncKey, device: &str, ask_for_full: bool) -> Result<SettingsSynced> {
    use crate::settings_sync::{Item, merge};
    let mut done = SettingsSynced::default();
    let found = read_topic(store, "settings", "all", key, device, &|plain| serde_json::from_slice::<Vec<Item>>(plain).is_ok())?;
    let mut sets: Vec<Vec<Item>> = Vec::new();
    let mut mine: Option<Vec<Item>> = None;
    for (from, plain) in &found.copies {
        let Ok(items) = serde_json::from_slice::<Vec<Item>>(plain) else { continue };
        if *from == device {
            mine = Some(items.clone());
        }
        sets.push(items);
        done.computers += 1;
    }
    done.broken = found.broken.clone();
    let local = library.synced_items(device)?;
    sets.push(local.clone());
    let merged = merge(&sets);
    // something changed here that this computer's stored copy doesn't say yet
    let own_new = merged.iter().any(|i| i.device == device && mine.as_ref().is_none_or(|m| !m.contains(i)));
    let answer = !local.is_empty() && found.sos.iter().any(|(_, from)| from != device);
    let store_now = store.writable() && (own_new || answer || found.mine_broken);
    if store_now {
        done.parts = store_topic(store, "settings", "all", key, device, &serde_json::to_vec(&merged)?, &found.my_items)?;
        done.stored = true;
    }
    if store.writable() {
        (done.asked, done.answered) = settle_sos(store, "settings:all", key, device, &found, store_now && answer, ask_for_full)?;
    }
    done.changed = library.apply_items(&merged, device)?;
    Ok(done)
}

/// Everything one sync did: per account its filing, per settings account its settings.
#[derive(Debug, Default)]
pub struct SyncAll {
    pub filing: Vec<(String, Result<FilingSynced>)>,
    pub settings: Vec<(String, Result<SettingsSynced>)>,
}

/// Where an account's envelopes can live on its own server.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    /// A Microsoft 365 mailbox's sync folder (None: the signed-in user's own; Some: a shared one).
    Graph(Option<String>),
    /// A Gmail account's hidden label.
    Gmail(String),
    /// An IMAP account's folder (one whose server Eudora may change).
    Imap(String),
}

/// Where an address's envelopes can live: its own server, when it has one sync can use.
/// `own`: the Microsoft 365 account signed in here (another Microsoft account is synced by its
/// own sign-in).
pub fn place_of(address: &str, kind: &str, own: Option<&str>, imap_writable: &[String]) -> Option<Place> {
    let address = address.to_lowercase();
    match kind {
        "graph" if own == Some(address.as_str()) => Some(Place::Graph(None)),
        "graph-shared" if own.is_some() => Some(Place::Graph(Some(address))),
        "gmail" => Some(Place::Gmail(address)),
        "imap" if imap_writable.contains(&address) => Some(Place::Imap(address)),
        _ => None,
    }
}

/// The home account: the one chosen here, else the Microsoft 365 account signed in, else the
/// first Gmail account, else the first IMAP account sync can use.
pub fn default_home(chosen: Option<&str>, accounts: &[(String, String)], own: Option<&str>, imap_writable: &[String]) -> Option<String> {
    if let Some(c) = chosen.map(str::trim).filter(|c| !c.is_empty()) {
        return Some(c.to_lowercase());
    }
    if let Some(o) = own {
        return Some(o.to_lowercase());
    }
    let first = |kind: &str| accounts.iter().find(|(a, k)| k == kind && place_of(a, k, own, imap_writable).is_some()).map(|(a, _)| a.to_lowercase());
    first("gmail").or_else(|| first("imap"))
}

impl Library {
    /// For Settings ▸ Downloading and sync: the accounts that can carry sync's data here (address, kind:
    /// Microsoft 365 and its shared mailboxes, Gmail, IMAP with Eudora's changes on), and the
    /// home account (the one chosen, else `default_home`).
    pub fn sync_carriers(&self) -> Result<(Option<String>, Vec<(String, String)>)> {
        let accounts = self.accounts_counted(false)?;
        let listed: Vec<(String, String)> = accounts.iter().filter_map(|a| a.address.clone().map(|x| (x.to_lowercase(), a.kind.clone()))).collect();
        let imap_writable: Vec<String> = self.imap_accounts()?.into_iter().filter(|c| c.writes()).map(|c| c.address.to_lowercase()).collect();
        // (the window doesn't sign in itself: the first Microsoft 365 account is taken as the one)
        let own = listed.iter().find(|(_, k)| k == "graph").map(|(a, _)| a.clone());
        let can: Vec<(String, String)> = listed.iter().filter(|(a, k)| place_of(a, k, own.as_deref(), &imap_writable).is_some()).cloned().collect();
        let home = default_home(self.setting(HOME_SETTING)?.as_deref(), &listed, own.as_deref(), &imap_writable);
        Ok((home, can))
    }
}

/// Opens a place's store and runs `f` with it.
fn with_store<T>(library: &Library, graph: &mut Option<&mut GraphClient>, place: &Place, f: impl FnOnce(&mut dyn SyncStore) -> Result<T>) -> Result<T> {
    match place {
        Place::Graph(mailbox) => {
            let client = graph.as_deref_mut().ok_or_else(|| Error::Other("not signed in to Microsoft 365 here".into()))?;
            let mut store = GraphStore::open(client, mailbox.clone())?;
            f(&mut store)
        }
        Place::Gmail(address) => f(&mut crate::sync_stores::GmailStore::open(address)?),
        Place::Imap(address) => {
            let config = library
                .imap_accounts()?
                .into_iter()
                .find(|c| c.address.eq_ignore_ascii_case(address))
                .ok_or_else(|| Error::Other(format!("{address} isn't set up as a mail account here")))?;
            f(&mut crate::sync_stores::ImapStore::open(&config)?)
        }
    }
}

/// Checks that an account can hold sync's data (`eudora-cli sync check`): a test item stored in its
/// sync folder or label, read back, and deleted. Needs no sync key. Returns what was found, in
/// words.
pub fn check_store(library: &Library, mut graph: Option<&mut GraphClient>, address: &str) -> Result<String> {
    let address = address.trim().to_lowercase();
    let own: Option<String> = graph.as_ref().map(|c| c.account.to_lowercase()).filter(|a| !a.is_empty());
    let accounts = library.accounts_counted(false)?;
    let kind = accounts.iter().find(|a| a.address.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(&address))).map(|a| a.kind.clone());
    let Some(kind) = kind else { return Err(Error::Other(format!("{address} isn't an account in this library"))) };
    let imap_writable: Vec<String> = library.imap_accounts()?.into_iter().filter(|c| c.writes()).map(|c| c.address.to_lowercase()).collect();
    let place = place_of(&address, &kind, own.as_deref(), &imap_writable)
        .ok_or_else(|| Error::Other(format!("{address} can't hold sync's data ({kind}: Microsoft 365 signed in here, Gmail, or IMAP with Eudora's changes on)")))?;
    let name = format!("{SYNC_SUBJECT} · check · {} · {}", upload_stamp(), crate::sync_envelope::device_id()?);
    let payload: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    with_store(library, &mut graph, &place, |store| {
        let before = store.list()?.len();
        store.put(&name, &payload)?;
        let after = store.list()?;
        let found = after.iter().find(|(_, n, _)| *n == name).ok_or_else(|| Error::Other("the test item was stored but isn't there when read back".into()))?;
        if found.2 != payload {
            return Err(Error::Other("the test item came back changed".into()));
        }
        let id = found.0.clone();
        store.delete(&id)?;
        let gone = !store.list()?.iter().any(|(_, n, _)| *n == name);
        Ok(format!(
            "{address} ({place:?}) can hold sync's data: a test item stored, read back unchanged, and {}. Items there before: {before}.",
            if gone { "deleted" } else { "NOT deleted (please report this)" }
        ))
    })
}

/// Syncs the filing of every account this computer has: each Microsoft 365 mailbox, Gmail
/// account and IMAP account in its own server's sync folder (or label), accounts with no
/// server sync can use here (POP3, classic Eudora archives, imported files) in the home
/// account's; then the settings through the home account and the others ticked. `graph`: the
/// Microsoft 365 sign-in, when there is one. Nothing happens until sync is set up (a sync key
/// on this computer). Returns, per account address, what happened.
/// `ask_for_full`: send an SOS for every account (`eudora-cli sync repair`).
pub fn sync_all(library: &Library, mut graph: Option<&mut GraphClient>, ask_for_full: bool) -> Result<SyncAll> {
    let Some(key) = SyncKey::load() else { return Ok(SyncAll::default()) };
    let device = crate::sync_envelope::device_id()?;
    let own: Option<String> = graph.as_ref().map(|c| c.account.to_lowercase()).filter(|a| !a.is_empty());
    let accounts = library.accounts_counted(false)?;
    let listed: Vec<(String, String)> = accounts.iter().filter_map(|a| a.address.clone().map(|x| (x.to_lowercase(), a.kind.clone()))).collect();
    let imap_writable: Vec<String> = library.imap_accounts()?.into_iter().filter(|c| c.writes()).map(|c| c.address.to_lowercase()).collect();
    let chosen = library.setting(HOME_SETTING)?;
    let home = default_home(chosen.as_deref(), &listed, own.as_deref(), &imap_writable);
    let kind_of = |address: &str| listed.iter().find(|(a, _)| a == address).map(|(_, k)| k.as_str()).unwrap_or("");
    let home_place = home.as_deref().and_then(|h| place_of(h, kind_of(h), own.as_deref(), &imap_writable));
    let mut out = SyncAll::default();
    for a in &accounts {
        let Some(address) = a.address.clone().map(|x| x.to_lowercase()) else { continue };
        if a.kind == "graph" && own.as_deref() != Some(address.as_str()) {
            continue; // another Microsoft account: its own sign-in syncs it
        }
        // its own server's, else the home account's
        let Some(place) = place_of(&address, &a.kind, own.as_deref(), &imap_writable).or_else(|| home_place.clone()) else { continue };
        let result = with_store(library, &mut graph, &place, |store| sync_account_with(library, store, &key, &device, a.id, &address, ask_for_full));
        out.filing.push((address, result));
    }
    // the settings: in the home account, and the others ticked
    let mut carriers: Vec<String> = home.into_iter().collect();
    let extra: Vec<String> = library.setting(SETTINGS_ACCOUNTS)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
    for e in extra.into_iter().map(|e| e.to_lowercase()) {
        if !carriers.contains(&e) {
            carriers.push(e);
        }
    }
    for address in carriers {
        let Some(place) = place_of(&address, kind_of(&address), own.as_deref(), &imap_writable) else {
            continue; // not an account this computer can store in
        };
        let result = with_store(library, &mut graph, &place, |store| sync_settings(library, store, &key, &device, ask_for_full));
        out.settings.push((address, result));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;
    use crate::{ImportOptions, import};
    use std::collections::BTreeMap;

    /// A mailbox's sync folder, in memory.
    struct Mem {
        items: BTreeMap<String, (String, Vec<u8>)>,
        next: u32,
        limit: usize,
    }

    impl Default for Mem {
        fn default() -> Mem {
            Mem { items: BTreeMap::new(), next: 0, limit: 1 << 30 }
        }
    }

    struct MemStore<'a>(&'a mut Mem);

    impl SyncStore for MemStore<'_> {
        fn list(&mut self) -> Result<Vec<(String, String, Vec<u8>)>> {
            Ok(self.0.items.iter().map(|(id, (n, b))| (id.clone(), n.clone(), b.clone())).collect())
        }
        fn put(&mut self, name: &str, bytes: &[u8]) -> Result<String> {
            self.0.next += 1;
            let id = format!("m{}", self.0.next);
            self.0.items.insert(id.clone(), (name.to_string(), bytes.to_vec()));
            Ok(id)
        }
        fn delete(&mut self, id: &str) -> Result<()> {
            self.0.items.remove(id);
            Ok(())
        }
        fn writable(&self) -> bool {
            true
        }
        fn max_item(&self) -> usize {
            self.0.limit
        }
    }

    fn library(dir: &std::path::Path, address: &str) -> (Library, i64, i64) {
        let mut library = Library::open(&dir.join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let files: Vec<_> = (1..=2)
            .map(|n| {
                let p = src.join(format!("{n}.eml"));
                std::fs::write(&p, format!("From: a@b.com\r\nSubject: m{n}\r\nMessage-ID: <{n}@b.com>\r\n\r\nhi\r\n")).unwrap();
                p
            })
            .collect();
        import(&mut library, &files, &ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let inbox: i64 = library.conn.query_row("SELECT folder FROM locations LIMIT 1", [], |r| r.get(0)).unwrap();
        let account: i64 = library.conn.query_row("SELECT account FROM folders WHERE id = ?1", [inbox], |r| r.get(0)).unwrap();
        library.conn.execute("UPDATE accounts SET kind = 'imap', address = ?2 WHERE id = ?1", rusqlite::params![account, address]).unwrap();
        library.conn.execute("UPDATE folders SET path = 'Inbox', name = 'Inbox', parent = NULL WHERE id = ?1", [inbox]).unwrap();
        library.conn.execute("INSERT INTO server_items (folder, server_id, message) SELECT folder, 'uid' || message, message FROM locations", []).unwrap();
        (library, account, inbox)
    }

    /// Gmail-only, IMAP-only and POP3 people sync too.
    #[test]
    fn every_kind_of_account_has_a_place_or_uses_the_home_account() {
        let imap = vec!["me@fastmail.com".to_string()];
        assert_eq!(place_of("Me@Example.com", "graph", Some("me@example.com"), &imap), Some(Place::Graph(None)));
        assert_eq!(place_of("other@example.com", "graph", Some("me@example.com"), &imap), None, "another Microsoft sign-in's");
        assert_eq!(place_of("noc@example.com", "graph-shared", Some("me@example.com"), &imap), Some(Place::Graph(Some("noc@example.com".into()))));
        assert_eq!(place_of("noc@example.com", "graph-shared", None, &imap), None, "no Microsoft sign-in here");
        assert_eq!(place_of("me@gmail.com", "gmail", None, &imap), Some(Place::Gmail("me@gmail.com".into())));
        assert_eq!(place_of("me@fastmail.com", "imap", None, &imap), Some(Place::Imap("me@fastmail.com".into())));
        assert_eq!(place_of("ro@isp.com", "imap", None, &imap), None, "Eudora's changes switched off on that server");
        assert_eq!(place_of("me@isp.com", "pop3", None, &imap), None);

        let accounts = |list: &[(&str, &str)]| list.iter().map(|(a, k)| (a.to_string(), k.to_string())).collect::<Vec<_>>();
        // chosen here wins; else Microsoft 365, else Gmail, else IMAP; POP3 alone: none
        assert_eq!(default_home(Some(" Me@Gmail.com "), &[], None, &imap).as_deref(), Some("me@gmail.com"));
        assert_eq!(default_home(None, &accounts(&[("me@gmail.com", "gmail")]), Some("me@example.com"), &imap).as_deref(), Some("me@example.com"));
        assert_eq!(default_home(None, &accounts(&[("me@fastmail.com", "imap"), ("me@gmail.com", "gmail")]), None, &imap).as_deref(), Some("me@gmail.com"));
        assert_eq!(default_home(None, &accounts(&[("me@isp.com", "pop3"), ("me@fastmail.com", "imap")]), None, &imap).as_deref(), Some("me@fastmail.com"));
        assert_eq!(default_home(None, &accounts(&[("me@isp.com", "pop3")]), None, &imap), None);
    }

    #[test]
    fn two_computers_sync_one_account_through_its_folder_and_strangers_are_ignored() {
        let key = SyncKey::generate().unwrap();
        let mut mailbox = Mem::default();
        let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, a_account, a_inbox) = library(a_dir.path(), "me@x.com");
        let (b, b_account, _) = library(b_dir.path(), "me@x.com");
        let m1 = |l: &Library| l.conn.query_row("SELECT id FROM messages WHERE subject = 'm1'", [], |r| r.get::<_, i64>(0)).unwrap();

        // A files m1 in Projects and syncs: its envelope is stored
        let projects = crate::db::ensure_folder(&a.conn, a_account, "Projects").unwrap();
        a.move_messages(&[m1(&a)], a_inbox, projects).unwrap();
        let r = sync_account(&a, &mut MemStore(&mut mailbox), &key, "pc-a", a_account, "Me@x.com").unwrap();
        assert!((r.stored, r.sent) == (true, 1), "{r:?}");
        // someone else's envelope in the same (shared) mailbox, and junk: ignored
        let stranger = SyncKey::generate().unwrap();
        MemStore(&mut mailbox).put(&part_name("filing", "me@x.com", &stranger, "pc-z", "1", 1, 1), &stranger.seal("filing:me@x.com", b"x").unwrap()).unwrap();
        MemStore(&mut mailbox).put(&part_name("filing", "me@x.com", &key, "pc-y", "1", 1, 1), b"garbage").unwrap();

        // B syncs: m1 is filed there too; the junk upload (pc-y) can't be used, so B asks for a
        // full copy (SOS)
        let r = sync_account(&b, &mut MemStore(&mut mailbox), &key, "pc-b", b_account, "me@x.com").unwrap();
        assert_eq!((r.computers, r.applied.filed), (1, 1), "{r:?}");
        assert_eq!(b.folder_paths(m1(&b)).unwrap(), vec!["Projects".to_string()]);
        assert!(!r.stored, "B had nothing new to say");
        assert!(r.asked && r.broken == vec!["pc-y".to_string()], "{r:?}");

        // A answers with its complete copy: the junk and the SOS go
        let r = sync_account(&a, &mut MemStore(&mut mailbox), &key, "pc-a", a_account, "me@x.com").unwrap();
        assert!(r.stored && r.answered == 1 && !r.asked, "{r:?}");
        assert!(!mailbox.items.values().any(|(n, _)| n.contains("pc-y") || n.contains(" · sos · ")));
        // and with nothing new, A doesn't store again; one copy per computer
        let r = sync_account(&a, &mut MemStore(&mut mailbox), &key, "pc-a", a_account, "me@x.com").unwrap();
        assert!(!r.stored, "{r:?}");
        assert_eq!(mailbox.items.values().filter(|(n, _)| n.contains("pc-a")).count(), 1, "one copy per computer");

        // a big log goes in parts, and is read back whole
        mailbox.limit = 64;
        let m2 = |l: &Library| l.conn.query_row("SELECT id FROM messages WHERE subject = 'm2'", [], |r| r.get::<_, i64>(0)).unwrap();
        a.move_messages(&[m2(&a)], a_inbox, projects).unwrap();
        let r = sync_account(&a, &mut MemStore(&mut mailbox), &key, "pc-a", a_account, "me@x.com").unwrap();
        assert!(r.stored && r.parts > 2, "{r:?}");
        assert_eq!(mailbox.items.values().filter(|(n, _)| n.contains("pc-a")).count(), r.parts, "the old copy is gone once every part is in");
        let r = sync_account(&b, &mut MemStore(&mut mailbox), &key, "pc-b", b_account, "me@x.com").unwrap();
        assert_eq!(r.applied.filed, 1, "{r:?}");
        assert_eq!(b.folder_paths(m2(&b)).unwrap(), vec!["Projects".to_string()]);

        // an upload in progress (a part missing, just now) isn't used: the last whole one is
        MemStore(&mut mailbox).put(&part_name("filing", "me@x.com", &key, "pc-a", &upload_stamp(), 1, 3), b"x").unwrap();
        let r = sync_account(&b, &mut MemStore(&mut mailbox), &key, "pc-b", b_account, "me@x.com").unwrap();
        assert!((r.computers, r.broken.len(), r.asked) == (1, 0, false), "{r:?}");

        // A's copy in the mailbox gets damaged: B can't use it and asks for a full copy (SOS)
        for (name, bytes) in mailbox.items.values_mut() {
            if name.contains("pc-a") {
                bytes.pop();
            }
        }
        let r = sync_account(&b, &mut MemStore(&mut mailbox), &key, "pc-b", b_account, "me@x.com").unwrap();
        assert!(r.asked && r.broken == vec!["pc-a".to_string()], "{r:?}");
        assert_eq!(b.folder_paths(m2(&b)).unwrap(), vec!["Projects".to_string()], "what B had applied stays");
        let r = sync_account(&a, &mut MemStore(&mut mailbox), &key, "pc-a", a_account, "me@x.com").unwrap();
        assert!(r.stored && r.answered == 1, "A keeps a complete copy of its own: stored again in full, the SOS answered: {r:?}");
        assert!(!mailbox.items.values().any(|(n, _)| n.contains(" · sos · ")), "the SOS is gone");
        let r = sync_account(&b, &mut MemStore(&mut mailbox), &key, "pc-b", b_account, "me@x.com").unwrap();
        assert!(r.broken.is_empty() && !r.asked && r.computers == 1, "{r:?}");

        // asked by hand (`eudora-cli sync repair`): an SOS even when all is well; A answers it
        let r = sync_account_with(&b, &mut MemStore(&mut mailbox), &key, "pc-b", b_account, "me@x.com", true).unwrap();
        assert!(r.asked);
        let r = sync_account(&a, &mut MemStore(&mut mailbox), &key, "pc-a", a_account, "me@x.com").unwrap();
        assert!(r.stored && r.answered == 1, "{r:?}");

        // settings travel through the same folder: A's look reaches B
        a.set_setting("ui:skin", "\"gmail\"").unwrap();
        let r = sync_settings(&a, &mut MemStore(&mut mailbox), &key, "pc-a", false).unwrap();
        assert!(r.stored, "{r:?}");
        let r = sync_settings(&b, &mut MemStore(&mut mailbox), &key, "pc-b", false).unwrap();
        assert_eq!((r.computers, r.changed, r.stored), (1, 1, false), "{r:?}");
        assert_eq!(b.setting("ui:skin").unwrap().as_deref(), Some("\"gmail\""));
        assert!(!sync_settings(&a, &mut MemStore(&mut mailbox), &key, "pc-a", false).unwrap().stored, "nothing new");

        // another account's sync doesn't touch this one
        let c_dir = tempfile::tempdir().unwrap();
        let (c, c_account, _) = library(c_dir.path(), "other@x.com");
        let r = sync_account(&c, &mut MemStore(&mut mailbox), &key, "pc-c", c_account, "other@x.com").unwrap();
        assert_eq!((r.computers, r.applied.filed), (0, 0));
    }
}
