//! The library database (SQLite, WAL): everything about each message except its raw bytes.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

pub const SCHEMA_VERSION: i64 = 27;

/// Version 21: folders listed in full with nothing missing, at the server's count then
/// (`listed_folders`). Folders whose count includes items Eudora leaves on the server (calendar
/// items in Deleted Items, meeting items, an archive's contacts) never matched the count of
/// messages stored, so they were listed again every pass (2026-10-18: passes took 80 minutes);
/// now one is passed over while the server's count is still the one it was listed at.
const LISTED_FOLDERS: &str = r#"
CREATE TABLE listed_folders (
    folder INTEGER PRIMARY KEY,
    total INTEGER NOT NULL,
    at INTEGER NOT NULL
);
"#;

/// Version 20: address books. `cards`: one contact as an account keeps it (`book` = its address
/// book's server id, "" = the default one; `version` = Graph's changeKey or a CardDAV ETag;
/// `card` = Eudora's reading of it, JSON; `original` = the server's own form, kept byte for
/// byte). `card_history`: every card a merge or a delete changed, as it was, so it can be undone.
const CARDS: &str = r#"
CREATE TABLE cards (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES accounts(id),
    book TEXT NOT NULL DEFAULT '',
    book_name TEXT NOT NULL DEFAULT 'Contacts',
    server_id TEXT,
    version TEXT,
    card TEXT NOT NULL,
    original TEXT,
    updated INTEGER NOT NULL,
    UNIQUE (account, server_id)
);
CREATE INDEX cards_account ON cards(account);
CREATE TABLE card_history (
    id INTEGER PRIMARY KEY,
    card INTEGER,
    account INTEGER NOT NULL,
    book TEXT NOT NULL DEFAULT '',
    server_id TEXT,
    original TEXT,
    card_json TEXT NOT NULL,
    reason TEXT NOT NULL,
    at INTEGER NOT NULL
);
"#;

/// Version 22: calendars and their events (Microsoft 365 / Outlook.com first; CalDAV and
/// Google later). `delta` = where Microsoft's change list for the calendar stands (its
/// deltaLink) and the time range it covers; `original` = the event as the server gave it.
const CALENDARS: &str = r#"
CREATE TABLE calendars (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES accounts(id),
    server_id TEXT NOT NULL,
    name TEXT NOT NULL,
    color TEXT,
    can_edit INTEGER NOT NULL DEFAULT 1,
    is_default INTEGER NOT NULL DEFAULT 0,
    shown INTEGER NOT NULL DEFAULT 1,
    delta TEXT,
    range_end INTEGER NOT NULL DEFAULT 0,
    updated INTEGER NOT NULL,
    UNIQUE (account, server_id)
);
CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    calendar INTEGER NOT NULL REFERENCES calendars(id),
    server_id TEXT NOT NULL,
    version TEXT,
    kind TEXT NOT NULL,
    series TEXT,
    start INTEGER NOT NULL,
    end INTEGER NOT NULL,
    all_day INTEGER NOT NULL DEFAULT 0,
    subject TEXT NOT NULL,
    location TEXT NOT NULL DEFAULT '',
    event TEXT NOT NULL,
    original TEXT,
    updated INTEGER NOT NULL,
    UNIQUE (calendar, server_id)
);
CREATE INDEX events_start ON events(start);
CREATE INDEX events_series ON events(series);
"#;

/// Version 23: reservations found in Travel mail (`travel`): flights put on the calendar,
/// hotel stays offered; each once, by its key.
const RESERVATIONS: &str = r#"
CREATE TABLE reservations (
    key TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    start INTEGER NOT NULL,
    end INTEGER NOT NULL,
    title TEXT NOT NULL,
    location TEXT NOT NULL DEFAULT '',
    details TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,
    message INTEGER NOT NULL,
    subject TEXT NOT NULL DEFAULT '',
    found INTEGER NOT NULL
);
"#;

/// Version 24: To Do tasks (`todo`): here and in Microsoft To Do (`server_id`, `list_id`); a task
/// about a message keeps it and its Message-ID (another computer finds it by that).
const TODOS: &str = r#"
CREATE TABLE todos (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    notes TEXT NOT NULL DEFAULT '',
    due INTEGER,
    done INTEGER NOT NULL DEFAULT 0,
    done_at INTEGER,
    important INTEGER NOT NULL DEFAULT 0,
    message INTEGER,
    message_id TEXT,
    list_id TEXT,
    list_name TEXT NOT NULL DEFAULT 'Tasks',
    server_id TEXT UNIQUE,
    version TEXT,
    deleted INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
CREATE INDEX todos_message ON todos(message);
"#;

/// Version 25: the calendar event Eudora made for a reservation (so it can be taken off again).
const RESERVATION_EVENTS: &str = "ALTER TABLE reservations ADD COLUMN event INTEGER;";

/// Version 26: each folder's messages and unread kept as they change: triggers keep it
/// current whichever program writes (a message filed, removed or moved, read or unread);
/// maintenance checks it against a recount now and then. Counting from scratch took ~3 s on
/// 2.4 million messages. What each background job did to each message (`job_log`): Activity's
/// Details, 30 days.
const JOB_LOG: &str = r#"
CREATE TABLE IF NOT EXISTS job_log (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    run INTEGER NOT NULL,
    at INTEGER NOT NULL,
    message INTEGER,
    what TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS job_log_run ON job_log (job, run);
CREATE INDEX IF NOT EXISTS job_log_at ON job_log (at);
"#;

const FOLDER_COUNTS: &str = r#"
CREATE TABLE IF NOT EXISTS folder_counts (folder INTEGER PRIMARY KEY, messages INTEGER NOT NULL DEFAULT 0, unread INTEGER NOT NULL DEFAULT 0);
DELETE FROM folder_counts;
INSERT INTO folder_counts (folder, messages, unread)
  SELECT l.folder, COUNT(*), SUM(m.flags & 1) FROM locations l JOIN messages m ON m.id = l.message GROUP BY l.folder;
CREATE TRIGGER IF NOT EXISTS folder_counts_in AFTER INSERT ON locations BEGIN
  INSERT INTO folder_counts (folder, messages, unread) VALUES (NEW.folder, 1, COALESCE((SELECT flags & 1 FROM messages WHERE id = NEW.message), 0))
  ON CONFLICT (folder) DO UPDATE SET messages = messages + 1, unread = unread + excluded.unread;
END;
CREATE TRIGGER IF NOT EXISTS folder_counts_out AFTER DELETE ON locations BEGIN
  UPDATE folder_counts SET messages = messages - 1, unread = unread - COALESCE((SELECT flags & 1 FROM messages WHERE id = OLD.message), 0) WHERE folder = OLD.folder;
END;
CREATE TRIGGER IF NOT EXISTS folder_counts_moved AFTER UPDATE OF folder ON locations WHEN OLD.folder <> NEW.folder BEGIN
  UPDATE folder_counts SET messages = messages - 1, unread = unread - COALESCE((SELECT flags & 1 FROM messages WHERE id = OLD.message), 0) WHERE folder = OLD.folder;
  INSERT INTO folder_counts (folder, messages, unread) VALUES (NEW.folder, 1, COALESCE((SELECT flags & 1 FROM messages WHERE id = NEW.message), 0))
  ON CONFLICT (folder) DO UPDATE SET messages = messages + 1, unread = unread + excluded.unread;
END;
CREATE TRIGGER IF NOT EXISTS folder_counts_read AFTER UPDATE OF flags ON messages WHEN (OLD.flags & 1) <> (NEW.flags & 1) BEGIN
  UPDATE folder_counts SET unread = unread + (NEW.flags & 1) - (OLD.flags & 1) WHERE folder IN (SELECT folder FROM locations WHERE message = NEW.id);
END;
"#;

/// Version 19: whitelist / blacklist entries belong to a mailbox (its address, lowercase; ""
/// = every mailbox): a shared mailbox learns its own. The table is made again with the new
/// column and its uniqueness; every entry so far applies everywhere.
const SENDER_RULES_PER_MAILBOX: &str = r#"
CREATE TABLE sender_rules_new (
    id INTEGER PRIMARY KEY,
    pattern TEXT NOT NULL,
    kind TEXT NOT NULL,
    action TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    added INTEGER NOT NULL,
    mailbox TEXT NOT NULL DEFAULT '',
    UNIQUE (pattern, kind, mailbox)
);
INSERT INTO sender_rules_new (id, pattern, kind, action, source, note, added, mailbox)
    SELECT id, pattern, kind, action, source, note, added, '' FROM sender_rules;
DROP TABLE sender_rules;
ALTER TABLE sender_rules_new RENAME TO sender_rules;
"#;

/// Added in version 18: installed plugins (`plugins`): each one's WebAssembly file in
/// `<library>/plugins/`, its checksum, its manifest, and the API client its permissions belong to.
const PLUGINS: &str = r#"
CREATE TABLE IF NOT EXISTS plugins (
    id TEXT PRIMARY KEY,             -- the manifest's id (com.example.invoices)
    client INTEGER NOT NULL,         -- api_clients.id: what it may do, and its log
    file TEXT NOT NULL,              -- in <library>/plugins/
    hash BLOB NOT NULL,              -- BLAKE3 of the file: checked before every run
    manifest TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    installed INTEGER NOT NULL
);
"#;

/// Added in version 17: the local API (`api`): the programs and plugins the user let in (with
/// what they may do), their requests waiting for the user, sends waiting for the user's OK,
/// and the log of everything they did. This computer's own: never synced, never backed up
/// with tokens usable elsewhere (only token hashes are kept).
const API: &str = r#"
CREATE TABLE IF NOT EXISTS api_clients (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    publisher TEXT NOT NULL,
    kind TEXT NOT NULL,              -- program, plugin
    token_hash BLOB UNIQUE,          -- BLAKE3 of the token; NULL until granted
    grants TEXT NOT NULL,            -- JSON: [{permission, accounts, folders}]
    log_level TEXT NOT NULL DEFAULT 'full',   -- full, changes, refusals
    send_without_asking INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL,
    expires INTEGER,
    last_used INTEGER,
    revoked INTEGER
);
CREATE TABLE IF NOT EXISTS api_requests (
    id INTEGER PRIMARY KEY,
    client INTEGER NOT NULL,         -- api_clients.id (not granted yet)
    secret_hash BLOB NOT NULL,       -- what the asker presents to collect its token once granted
    reason TEXT NOT NULL,
    asked TEXT NOT NULL,             -- JSON: [{permission, accounts, folders}]
    created INTEGER NOT NULL,
    answer TEXT,                     -- granted, refused
    token TEXT                       -- handed over once, then cleared
);
CREATE TABLE IF NOT EXISTS api_sends (
    id INTEGER PRIMARY KEY,
    client INTEGER NOT NULL,
    draft TEXT NOT NULL,             -- JSON Draft
    created INTEGER NOT NULL,
    answer TEXT,                     -- sent, refused
    answered INTEGER
);
CREATE TABLE IF NOT EXISTS api_log (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    client INTEGER,
    method TEXT NOT NULL,
    write INTEGER NOT NULL,
    allowed INTEGER NOT NULL,
    detail TEXT NOT NULL             -- what it touched (ids, folders, counts), never the mail itself
);
CREATE INDEX IF NOT EXISTS api_log_at ON api_log (at);
CREATE INDEX IF NOT EXISTS api_log_client ON api_log (client, at);
"#;

/// Added in version 16: each message's labels (Outlook categories; `marks`). Status and the
/// priority set by hand are bits of `messages.flags`.
const CATEGORIES: &str = r#"
CREATE TABLE IF NOT EXISTS categories (
    message INTEGER NOT NULL,
    label TEXT NOT NULL,
    UNIQUE (message, label)
);
CREATE INDEX IF NOT EXISTS categories_label ON categories (label, message);
"#;

/// Added in version 15: blocklist answers (`blocklists`), kept an hour.
const BLOCKLISTS: &str = r#"
CREATE TABLE IF NOT EXISTS blocklist_cache (
    list TEXT NOT NULL,              -- zen, dbl, zrd
    name TEXT NOT NULL,              -- the reversed IP, or the domain
    result TEXT,                     -- what the listing means; NULL: not listed
    expires INTEGER NOT NULL,
    PRIMARY KEY (list, name)
) WITHOUT ROWID;
"#;

/// Added in version 14: what's known of senders' domains (`reputation`): when each was
/// registered (RDAP) and when its mail first came.
const DOMAINS: &str = r#"
CREATE TABLE IF NOT EXISTS domains (
    domain TEXT PRIMARY KEY,         -- an organization's domain (example.com)
    registered INTEGER,              -- unix seconds; NULL: not known (yet)
    first_seen INTEGER,
    checked INTEGER NOT NULL
);
"#;

/// Added in version 13: the spam filter's learning (`bayes`), one model per mailbox: how often
/// each token was in spam and in good mail, and which messages were learned how.
const BAYES: &str = r#"
CREATE TABLE IF NOT EXISTS bayes_tokens (
    account INTEGER NOT NULL REFERENCES accounts(id),
    token INTEGER NOT NULL,
    spam INTEGER NOT NULL,
    ham INTEGER NOT NULL,
    PRIMARY KEY (account, token)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS bayes_trained (
    message INTEGER NOT NULL REFERENCES messages(id),
    account INTEGER NOT NULL REFERENCES accounts(id),
    label INTEGER NOT NULL,          -- 1 spam, 0 good
    PRIMARY KEY (message, account)
) WITHOUT ROWID;
"#;

/// Added in version 12: text from inside attachments (`extract`: a PDF's own text, later
/// pictures read by OCR), per message and part; added to what search finds.
const EXTRACTED: &str = r#"
CREATE TABLE IF NOT EXISTS extracted (
    message INTEGER NOT NULL REFERENCES messages(id),
    part TEXT NOT NULL,
    source TEXT NOT NULL,            -- pdf, ocr
    text TEXT NOT NULL,
    PRIMARY KEY (message, part)
) WITHOUT ROWID;
"#;

/// Added in version 11: payments linked to their receipts (`links`): the dollar amounts found in
/// payments and receipts (-1: read, none found), and the pairs that match.
const LINKS: &str = r#"
CREATE TABLE IF NOT EXISTS amounts (
    message INTEGER NOT NULL REFERENCES messages(id),
    cents INTEGER NOT NULL,
    PRIMARY KEY (message, cents)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS amounts_cents ON amounts (cents, message);
CREATE TABLE IF NOT EXISTS links (
    payment INTEGER NOT NULL REFERENCES messages(id),
    receipt INTEGER NOT NULL REFERENCES messages(id),
    cents INTEGER NOT NULL,
    PRIMARY KEY (payment, receipt)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS links_receipt ON links (receipt);
"#;

/// Added in version 10: Synthetic Views (`views`): which messages each view holds, by date so
/// a view pages like a folder.
const TAGS: &str = r#"
CREATE TABLE IF NOT EXISTS tags (
    tag TEXT NOT NULL,               -- receipts, invoices, renewals, bank, new-accounts, 2fa…
    date INTEGER NOT NULL,           -- copy of messages.date
    message INTEGER NOT NULL REFERENCES messages(id),
    PRIMARY KEY (tag, date, message)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS tags_message ON tags (message);
"#;

/// Added in version 9: classic Eudora's attachment files (`old_attachments`): every file a
/// classic message refers to ("Attachment Converted: …", embedded pictures, X-Attachments),
/// and, once found, the file stored in the packs (`attachment_files`: where an attachment's
/// own bytes are, for attachments that aren't inside a message).
const OLD_ATTACHMENTS: &str = r#"
CREATE TABLE IF NOT EXISTS attachment_files (
    attachment INTEGER PRIMARY KEY REFERENCES attachments(id),
    pack INTEGER NOT NULL,
    pack_offset INTEGER NOT NULL,
    pack_len INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS old_attachments (
    message INTEGER NOT NULL REFERENCES messages(id),
    part TEXT NOT NULL,              -- eudora:1, eudora:2 … in the order the message names them
    kind TEXT NOT NULL,              -- attached, embedded, sent
    path TEXT NOT NULL,              -- as the message wrote it
    name TEXT NOT NULL,
    attachment INTEGER REFERENCES attachments(id),   -- the file, once found
    found TEXT,                      -- where it was found
    PRIMARY KEY (message, part)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS old_attachments_missing ON old_attachments (name) WHERE attachment IS NULL;
"#;

/// Added in version 8: filing made on this computer that no server holds (archive mail filed
/// anywhere, IMAP / POP3 / imported mail moved here), waiting to join its account's filing log
/// (`filing_sync`; the sync takes them and deletes them). `folder` is the account-relative
/// path, `filing_sync::AS_ON_SERVER` ("") when the filing was undone.
const FILING: &str = r#"
CREATE TABLE IF NOT EXISTS filing_pending (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES accounts(id),
    message INTEGER NOT NULL REFERENCES messages(id),
    folder TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS filing_pending_account ON filing_pending (account, id);
"#;

/// Added in version 6: the people for address completion, made from the mail (who the user
/// wrote to, who wrote to them), rebuilt now and then (`Library::rebuild_contacts`).
const CONTACTS: &str = r#"
CREATE TABLE IF NOT EXISTS contacts (
    address TEXT PRIMARY KEY,        -- lowercase
    name TEXT NOT NULL,
    sent INTEGER NOT NULL,           -- messages the user sent them
    received INTEGER NOT NULL,       -- messages they sent the user
    last INTEGER NOT NULL            -- the latest of those (unix seconds)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS contacts_name ON contacts (name COLLATE NOCASE);
"#;

/// Added in version 7: sorting mail out of the Inbox (`triage`).
/// `sender_rules`: the whitelist (`allow`: automatic from the user's own mail, or added by
/// them), entries of the automatic whitelist the user took out (`not-allow`: never added back),
/// and the blacklist (`block`, each with its own action). `triage`: what was decided for a
/// message, and why.
const TRIAGE: &str = r#"
CREATE TABLE IF NOT EXISTS sender_rules (
    id INTEGER PRIMARY KEY,
    pattern TEXT NOT NULL,           -- someone@example.com, or @example.com (a domain and its subdomains); lowercase
    kind TEXT NOT NULL,              -- allow | block | not-allow
    action TEXT NOT NULL DEFAULT '', -- block: what to do (JSON, triage::Action)
    source TEXT NOT NULL,            -- auto | user
    note TEXT NOT NULL DEFAULT '',
    added INTEGER NOT NULL,
    UNIQUE (pattern, kind)
);
CREATE TABLE IF NOT EXISTS triage (
    message INTEGER PRIMARY KEY,
    category TEXT NOT NULL,          -- priority, people, list, bulk, spam, blocked, rule, other
    priority INTEGER NOT NULL DEFAULT 0,
    reasons TEXT NOT NULL,           -- JSON: why
    decided INTEGER NOT NULL,
    applied INTEGER NOT NULL DEFAULT 0
);
"#;

/// Added in version 5: a folder's id on its server (Exchange folder ids), so changes made in
/// Eudora (moves) can be applied there.
const FOLDER_SERVER_IDS: &str = "ALTER TABLE folders ADD COLUMN server_id TEXT;";

/// Added in version 4: every original copy of a message, byte for byte. A message's own
/// bytes (`messages.pack…`) are its first copy; a copy merged by Message-ID from another
/// source keeps its bytes here too, and an Online Archive item keeps Exchange's export
/// (`format` "exchange-export"), which its MIME was made from. Identical bytes are stored
/// once (the location records every place they were).
const COPIES: &str = r#"
CREATE TABLE IF NOT EXISTS copies (
    id INTEGER PRIMARY KEY,
    message INTEGER NOT NULL REFERENCES messages(id),
    folder INTEGER REFERENCES folders(id),   -- where this copy came from
    server_id TEXT,                          -- its id there (downloads)
    format TEXT NOT NULL,                    -- mime, exchange-export
    content_hash BLOB NOT NULL UNIQUE,       -- BLAKE3 of these bytes
    size INTEGER NOT NULL,
    pack INTEGER NOT NULL,
    pack_offset INTEGER NOT NULL,
    pack_len INTEGER NOT NULL,
    added INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS copies_message ON copies (message);
CREATE INDEX IF NOT EXISTS copies_source ON copies (folder, server_id);
"#;

/// Each message's own bytes as its first copy, for messages that have none yet (a library
/// from before version 4, or messages an older Eudora stored since). Run when a library is
/// opened for writing.
const FIRST_COPIES: &str = r#"
INSERT OR IGNORE INTO copies (message, folder, server_id, format, content_hash, size, pack, pack_offset, pack_len, added)
SELECT m.id,
       (SELECT l.folder FROM locations l WHERE l.message = m.id ORDER BY l.id LIMIT 1),
       (SELECT l.server_id FROM locations l WHERE l.message = m.id ORDER BY l.id LIMIT 1),
       'mime', m.content_hash, m.size, m.pack, m.pack_offset, m.pack_len, m.added
FROM messages m WHERE NOT EXISTS (SELECT 1 FROM copies c WHERE c.message = m.id);
"#;

/// Gives every message its first copy (see `FIRST_COPIES`); returns how many were added.
pub fn fill_first_copies(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(FIRST_COPIES, [])?)
}

/// Added in version 3: what the user calls an account (shown instead of its address; the
/// address and type stay in the tooltip), and where its mail came from ("C:\OldMail").
const ACCOUNT_LABELS: &str = r#"
ALTER TABLE accounts ADD COLUMN label TEXT;
ALTER TABLE accounts ADD COLUMN source TEXT;
"#;

/// What an account's `kind` is called in the UI.
pub fn kind_name(kind: &str) -> &'static str {
    match kind {
        "graph" => "Microsoft 365",
        "graph-shared" => "Microsoft 365 (shared)",
        "gmail" => "Gmail",
        "imap" => "IMAP",
        "pop3" => "POP3",
        "eudora" => "classic Eudora",
        "mbox" => "mailbox files",
        _ => "imported files",
    }
}

/// Added in version 2: which server items are already in the library, per folder (several
/// server items can be one message: copies merged by Message-ID, or the same message twice in
/// a folder). Downloads skip what's here, so they resume where they stopped.
const SERVER_ITEMS: &str = r#"
CREATE TABLE IF NOT EXISTS server_items (
    folder INTEGER NOT NULL REFERENCES folders(id),
    server_id TEXT NOT NULL,
    message INTEGER NOT NULL REFERENCES messages(id),
    PRIMARY KEY (folder, server_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS server_items_message ON server_items (message);
"#;

/// Address roles in `message_addresses`.
pub const ROLE_FROM: i64 = 0;
pub const ROLE_TO: i64 = 1;
pub const ROLE_CC: i64 = 2;
pub const ROLE_BCC: i64 = 3;

/// Reference kinds in `message_refs`.
pub const REF_IN_REPLY_TO: i64 = 0;
pub const REF_REFERENCES: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,            -- local, graph, gmail, imap, pop3
    name TEXT NOT NULL,
    address TEXT,
    hidden INTEGER NOT NULL DEFAULT 0,  -- not listed in the folder pane (still synced and searched)
    position INTEGER NOT NULL DEFAULT 0,  -- order in the folder pane
    created INTEGER NOT NULL
);

CREATE TABLE folders (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES accounts(id),
    path TEXT NOT NULL,            -- "Imports/Old Mail/Inbox"
    name TEXT NOT NULL,
    parent INTEGER REFERENCES folders(id),
    role TEXT,                     -- inbox, sent, drafts, junk, trash, archive (unified folders)
    hidden INTEGER NOT NULL DEFAULT 0,  -- not listed (still synced and searched)
    UNIQUE (account, path)
);
CREATE INDEX folders_role ON folders (role);

-- One row per logical message. The raw bytes are in the packs.
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    content_hash BLOB NOT NULL UNIQUE,   -- BLAKE3 of the raw message
    message_id TEXT NOT NULL,
    message_id_made INTEGER NOT NULL DEFAULT 0,
    date INTEGER NOT NULL,               -- unix seconds UTC, 0 = unknown
    date_source INTEGER NOT NULL,
    from_display TEXT NOT NULL,
    subject TEXT NOT NULL,
    snippet TEXT NOT NULL,
    size INTEGER NOT NULL,
    attachment_count INTEGER NOT NULL,
    flags INTEGER NOT NULL DEFAULT 0,
    thread INTEGER,
    list_id TEXT,
    parse_status INTEGER NOT NULL,       -- 0 ok, 1 fixed, 2 unreadable
    pack INTEGER NOT NULL,
    pack_offset INTEGER NOT NULL,
    pack_len INTEGER NOT NULL,
    added INTEGER NOT NULL
);
CREATE INDEX messages_message_id ON messages (message_id);
CREATE INDEX messages_date ON messages (date, id);

-- Where each message lives. Folder views read only this covering index.
CREATE TABLE locations (
    id INTEGER PRIMARY KEY,
    message INTEGER NOT NULL REFERENCES messages(id),
    folder INTEGER NOT NULL REFERENCES folders(id),
    date INTEGER NOT NULL,               -- copy of messages.date
    server_id TEXT,
    flags INTEGER NOT NULL DEFAULT 0,
    UNIQUE (folder, message)
);
CREATE INDEX locations_folder_date ON locations (folder, date, message);
CREATE INDEX locations_message ON locations (message);

CREATE TABLE addresses (id INTEGER PRIMARY KEY, address TEXT NOT NULL UNIQUE);  -- lowercase
CREATE TABLE message_addresses (
    message INTEGER NOT NULL,
    role INTEGER NOT NULL,               -- 0 from, 1 to, 2 cc, 3 bcc
    position INTEGER NOT NULL,
    address INTEGER NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (message, role, position)
) WITHOUT ROWID;
CREATE INDEX message_addresses_address ON message_addresses (address, message);

CREATE TABLE message_refs (
    message INTEGER NOT NULL,
    kind INTEGER NOT NULL,               -- 0 In-Reply-To, 1 References
    position INTEGER NOT NULL,
    ref TEXT NOT NULL,
    PRIMARY KEY (message, kind, position)
) WITHOUT ROWID;
CREATE INDEX message_refs_ref ON message_refs (ref);

-- One row per distinct attachment content; attachment_refs = every message part holding it.
CREATE TABLE attachments (
    id INTEGER PRIMARY KEY,
    hash BLOB NOT NULL UNIQUE,           -- BLAKE3 of the decoded bytes
    size INTEGER NOT NULL,
    mime TEXT NOT NULL,
    text_indexed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE attachment_refs (
    message INTEGER NOT NULL,
    part TEXT NOT NULL,
    attachment INTEGER NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,                  -- mime, uuencode, binhex
    server_ref TEXT,
    PRIMARY KEY (message, part)
) WITHOUT ROWID;
CREATE INDEX attachment_refs_attachment ON attachment_refs (attachment);

-- This computer only, never synced: attachment files written to the Attachments folder.
CREATE TABLE local_files (
    attachment INTEGER PRIMARY KEY,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL
);

CREATE TABLE sync_state (folder INTEGER PRIMARY KEY, cursor TEXT NOT NULL);
CREATE TABLE pending_ops (
    id INTEGER PRIMARY KEY,
    created INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
);
CREATE TABLE settings_items (
    key TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    device TEXT NOT NULL,
    modified INTEGER NOT NULL,
    value TEXT NOT NULL
);

CREATE TABLE imports (
    id INTEGER PRIMARY KEY,
    source_key TEXT NOT NULL,
    sources TEXT NOT NULL,
    started INTEGER NOT NULL,
    finished INTEGER,
    status TEXT NOT NULL,                -- running, done, canceled, failed
    checkpoint TEXT,
    counts TEXT
);
CREATE INDEX imports_source ON imports (source_key, status);
CREATE TABLE import_problems (
    id INTEGER PRIMARY KEY,
    import INTEGER NOT NULL,
    message INTEGER,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX import_problems_import ON import_problems (import, kind);

-- Messages whose index entry must be rewritten (a new folder for an existing message).
CREATE TABLE index_pending (message INTEGER PRIMARY KEY);
"#;

/// How much memory the database may use: setting `memory` = normal / more / most. Each
/// connection's own cache (the window has two, the download one) and how much of the file is read
/// through memory mapping (shared through the system's file cache; this SQLite maps at most ~2
/// GB).
pub const MEMORY: &str = "memory";

/// (cache per connection, mapped) in bytes for a memory choice.
pub fn memory_sizes(choice: &str) -> (i64, i64) {
    const MB: i64 = 1024 * 1024;
    match choice {
        "more" => (1024 * MB, 2047 * MB),
        "most" => (2048 * MB, 2047 * MB),
        _ => (256 * MB, 1024 * MB),
    }
}

/// Applies the memory choice kept in the library (at every open; at once when changed).
pub fn apply_memory(conn: &Connection) -> Result<()> {
    let choice: Option<String> = conn.query_row("SELECT value FROM settings_items WHERE key = ?1", [MEMORY], |r| r.get(0)).optional().unwrap_or(None);
    let choice = choice.and_then(|v| serde_json::from_str::<String>(&v).ok()).unwrap_or_default();
    let (cache, mapped) = memory_sizes(&choice);
    conn.execute_batch(&format!("PRAGMA cache_size = -{}; PRAGMA mmap_size = {mapped};", cache / 1024))?;
    Ok(())
}

/// The computer's memory in bytes (Windows; None elsewhere for now).
pub fn total_memory() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        // SAFETY: the structure is ours, sized as Windows asks
        if unsafe { GlobalMemoryStatusEx(&mut status) }.is_ok() {
            return Some(status.ullTotalPhys);
        }
    }
    None
}

/// Opens (creating if needed) the database with the settings a large library needs.
pub fn open(path: &std::path::Path, now: i64) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // (busy_timeout: the viewer and the download share the database; the viewer's address
    // completion rebuild holds it for more than 10 s on millions of messages, and the download
    // once stopped on "database is locked" at its start, 2026-10-09)
    conn.execute_batch(
        "PRAGMA page_size = 8192;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -262144;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 1073741824;
         PRAGMA busy_timeout = 120000;",
    )?;
    let exists: bool = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'", [], |_| Ok(true))
        .optional()?
        .unwrap_or(false);
    if exists {
        upgrade(&conn)?;
    } else {
        conn.execute_batch("BEGIN")?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(SERVER_ITEMS)?;
        conn.execute_batch(ACCOUNT_LABELS)?;
        conn.execute_batch(COPIES)?;
        conn.execute_batch(FOLDER_SERVER_IDS)?;
        conn.execute_batch(CONTACTS)?;
        conn.execute_batch(TRIAGE)?;
        conn.execute_batch(FILING)?;
        conn.execute_batch(OLD_ATTACHMENTS)?;
        conn.execute_batch(TAGS)?;
        conn.execute_batch(LINKS)?;
        conn.execute_batch(EXTRACTED)?;
        conn.execute_batch(BAYES)?;
        conn.execute_batch(DOMAINS)?;
        conn.execute_batch(BLOCKLISTS)?;
        conn.execute_batch(CATEGORIES)?;
        conn.execute_batch(API)?;
        conn.execute_batch(PLUGINS)?;
        conn.execute_batch(SENDER_RULES_PER_MAILBOX)?;
        conn.execute_batch(CARDS)?;
        conn.execute_batch(LISTED_FOLDERS)?;
        conn.execute_batch(CALENDARS)?;
        conn.execute_batch(RESERVATIONS)?;
        conn.execute_batch(TODOS)?;
        conn.execute_batch(RESERVATION_EVENTS)?;
        conn.execute_batch(FOLDER_COUNTS)?;
        conn.execute_batch(JOB_LOG)?;
        let library_id = blake3::hash(format!("{now}-{}-{:?}", std::process::id(), path).as_bytes()).to_hex();
        conn.execute("INSERT INTO meta VALUES ('schema_version', ?1)", params![SCHEMA_VERSION.to_string()])?;
        conn.execute("INSERT INTO meta VALUES ('library_id', ?1)", params![&library_id[..32]])?;
        conn.execute("INSERT INTO meta VALUES ('created', ?1)", params![now.to_string()])?;
        conn.execute_batch("COMMIT")?;
    }
    // more memory for speed, when the user chose it
    let _ = apply_memory(&conn);
    Ok(conn)
}

/// Brings an older library's database up to `SCHEMA_VERSION` (only adds things). A reader
/// (`Access::Read`) may run this too: it's one short transaction, and SQLite serializes it.
fn upgrade(conn: &Connection) -> Result<()> {
    let version: i64 = meta(conn, "schema_version")?.and_then(|v| v.parse().ok()).unwrap_or(1);
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    if version < 2 {
        conn.execute_batch(SERVER_ITEMS)?;
    }
    if version < 3 {
        conn.execute_batch(ACCOUNT_LABELS)?;
    }
    if version < 4 {
        conn.execute_batch(COPIES)?; // (first copies are filled in when opened for writing)
    }
    if version < 5 {
        conn.execute_batch(FOLDER_SERVER_IDS)?;
    }
    if version < 6 {
        conn.execute_batch(CONTACTS)?;
    }
    if version < 7 {
        conn.execute_batch(TRIAGE)?;
    }
    if version < 8 {
        conn.execute_batch(FILING)?;
    }
    if version < 9 {
        conn.execute_batch(OLD_ATTACHMENTS)?;
    }
    if version < 10 {
        conn.execute_batch(TAGS)?;
    }
    if version < 11 {
        conn.execute_batch(LINKS)?;
    }
    if version < 12 {
        conn.execute_batch(EXTRACTED)?;
    }
    if version < 13 {
        conn.execute_batch(BAYES)?;
    }
    if version < 14 {
        conn.execute_batch(DOMAINS)?;
    }
    if version < 15 {
        conn.execute_batch(BLOCKLISTS)?;
    }
    if version < 16 {
        conn.execute_batch(CATEGORIES)?;
    }
    if version < 17 {
        conn.execute_batch(API)?;
    }
    if version < 18 {
        conn.execute_batch(PLUGINS)?;
    }
    if version < 19 {
        conn.execute_batch(SENDER_RULES_PER_MAILBOX)?;
    }
    if version < 20 {
        conn.execute_batch(CARDS)?;
    }
    if version < 21 {
        conn.execute_batch(LISTED_FOLDERS)?;
    }
    if version < 22 {
        conn.execute_batch(CALENDARS)?;
    }
    if version < 23 {
        conn.execute_batch(RESERVATIONS)?;
    }
    if version < 24 {
        conn.execute_batch(TODOS)?;
    }
    if version < 25 {
        conn.execute_batch(RESERVATION_EVENTS)?;
    }
    if version < 26 {
        conn.execute_batch(FOLDER_COUNTS)?;
    }
    if version < 27 {
        conn.execute_batch(JOB_LOG)?;
    }
    conn.execute("UPDATE meta SET value = ?1 WHERE key = 'schema_version'", params![SCHEMA_VERSION.to_string()])?;
    conn.execute_batch("COMMIT")?;
    Ok(())
}

/// The account for mail imported from files: by its address when known (classic Eudora's
/// own address from its settings), else the "imported files" account. Created on first use.
/// `separate`: a new account even when this address has one (the user chose to keep this
/// archive apart).
pub fn import_account(conn: &Connection, kind: &str, address: Option<&str>, name: &str, source: &str, separate: bool, now: i64) -> Result<i64> {
    let Some(address) = address else { return local_account(conn, now) };
    if !separate {
        if let Some((id, _)) = find_account(conn, kind, address)? {
            return Ok(id);
        }
    }
    let position: i64 = conn.query_row("SELECT COALESCE(MAX(position), 0) + 1 FROM accounts", [], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO accounts (kind, name, address, position, created, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![kind, name, address, position, now, source],
    )?;
    Ok(conn.last_insert_rowid())
}

/// An account of this kind for this address (its id and where its mail came from).
pub fn find_account(conn: &Connection, kind: &str, address: &str) -> Result<Option<(i64, Option<String>)>> {
    Ok(conn
        .query_row(
            "SELECT id, source FROM accounts WHERE kind = ?1 AND address = ?2 COLLATE NOCASE ORDER BY id LIMIT 1",
            params![kind, address],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// The account for a Microsoft 365 / Outlook.com mailbox (created on first use): `kind`
/// "graph" for the user's own, "graph-shared" for a mailbox shared with them.
pub fn graph_account(conn: &Connection, kind: &str, address: &str, name: &str, now: i64) -> Result<i64> {
    if let Some(id) = conn
        .query_row("SELECT id FROM accounts WHERE kind = ?1 AND address = ?2 COLLATE NOCASE", params![kind, address], |r| r.get(0))
        .optional()?
    {
        return Ok(id);
    }
    let position: i64 = conn.query_row("SELECT COALESCE(MAX(position), 0) + 1 FROM accounts", [], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO accounts (kind, name, address, position, created) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![kind, if name.is_empty() { address } else { name }, address, position, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The shared mailboxes added to the library (address, name).
pub fn shared_mailboxes(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT address, name FROM accounts WHERE kind = 'graph-shared' AND address IS NOT NULL ORDER BY position, id")?;
    Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
}

pub fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional()?)
}

/// The local account that holds imported mail (created on first use).
pub fn local_account(conn: &Connection, now: i64) -> Result<i64> {
    if let Some(id) = conn
        .query_row("SELECT id FROM accounts WHERE kind = 'local' ORDER BY id LIMIT 1", [], |r| r.get(0))
        .optional()?
    {
        return Ok(id);
    }
    conn.execute("INSERT INTO accounts (kind, name, created) VALUES ('local', 'Imported files', ?1)", params![now])?;
    Ok(conn.last_insert_rowid())
}

/// The folder at `path` ("A/B/C"), creating it and its parents.
pub fn ensure_folder(conn: &Connection, account: i64, path: &str) -> Result<i64> {
    let mut parent: Option<i64> = None;
    let mut so_far = String::new();
    let mut id = 0;
    for name in path.split('/').map(str::trim).filter(|s| !s.is_empty()) {
        if !so_far.is_empty() {
            so_far.push('/');
        }
        so_far.push_str(name);
        let existing: Option<i64> = conn
            .query_row("SELECT id FROM folders WHERE account = ?1 AND path = ?2", params![account, so_far], |r| r.get(0))
            .optional()?;
        id = match existing {
            Some(id) => id,
            None => {
                conn.execute(
                    "INSERT INTO folders (account, path, name, parent) VALUES (?1, ?2, ?3, ?4)",
                    params![account, so_far, name, parent],
                )?;
                conn.last_insert_rowid()
            }
        };
        parent = Some(id);
    }
    Ok(id)
}

pub fn find_folder(conn: &Connection, path: &str) -> Result<Option<i64>> {
    let path = path.split('/').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/");
    Ok(conn
        .query_row("SELECT id FROM folders WHERE path = ?1 ORDER BY account LIMIT 1", params![path], |r| r.get(0))
        .optional()?)
}
