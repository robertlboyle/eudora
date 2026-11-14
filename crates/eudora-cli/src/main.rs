//! `eudora-cli`: the command-line tool for Eudora's core (import, search, check, benchmark, and
//! the Microsoft account connection), until the UI exists.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use eudora_core::graph::GraphClient;
use eudora_core::progress::thousands;
use eudora_core::{Access, ImportOptions, Library, PageCursor, Progress, SortOrder};

#[derive(Parser)]
#[command(name = "eudora-cli", version, about = "Eudora: the mail library from the command line")]
struct Cli {
    /// The library folder (never inside Dropbox or another sync folder).
    #[arg(long, short = 'l', env = "EUDORA_LIBRARY", global = true)]
    library: Option<PathBuf>,
    /// Also keep a record in this file (appended): every event, and the progress once a minute.
    #[arg(long, global = true)]
    log: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import mbox / Eudora .mbx / EML files or folders of them.
    Import {
        paths: Vec<PathBuf>,
        /// Folder to import into (default: the account's top; "Imports/<name>" for files with no account).
        #[arg(long)]
        into: Option<String>,
        /// Start over instead of resuming an unfinished import of the same files.
        #[arg(long)]
        restart: bool,
        /// Whose mail it is (an address). Default: a classic Eudora folder's own address.
        #[arg(long)]
        account: Option<String>,
        /// That address already has an account: add this archive to it.
        #[arg(long, conflicts_with = "separate")]
        merge: bool,
        /// That address already has an account: keep this archive as an account of its own.
        #[arg(long)]
        separate: bool,
    },
    /// Every account (mailboxes, old archives) with its type and message count.
    Accounts,
    /// Gives an account a name of your own (shown instead of its address); "" goes back to the address.
    Name { account: String, name: String },
    /// Changes the address of an account made from files (a classic Eudora archive): ACCOUNT is
    /// its number in `eudora-cli accounts`.
    Readdress { account: i64, address: String },
    /// Maintenance for libraries made before imports recognized classic Eudora folders: the
    /// imported-files account becomes the Eudora folder's own account.
    AdoptEudora {
        /// The classic Eudora folder the mail came from (its eudora.ini says whose it is).
        from: PathBuf,
        /// The folder the import put everything under ("Eudora 2005"), moved to the account's top.
        #[arg(long)]
        root: Option<String>,
    },
    /// Removes a folder whose every message is also in another folder (nothing is lost).
    RemoveCopies { folder: String },
    /// The whole library as one file, to set up another computer (then the next download
    /// catches up only what's new); made while Eudora runs.
    SeedCreate { out: PathBuf },
    /// Sets up a library from a seed (into a new folder: --library is where it goes).
    SeedRestore { seed: PathBuf },
    /// Rules (classic Eudora's filters).
    Rules {
        #[command(subcommand)]
        command: RulesCommand,
    },
    /// Contacts and calendars on CardDAV / CalDAV servers (iCloud, Fastmail, Nextcloud, hosting
    /// companies). The password: the variable named by --password-env, else the one kept for the
    /// account, else the IMAP account's of the same address.
    Dav {
        #[command(subcommand)]
        command: DavCommand,
    },
    /// Search everything: words (AND by default), OR, NOT, "phrases", from:, subject:, attachment:, date:[a TO b].
    Search {
        query: String,
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
        /// Best matches first instead of newest first.
        #[arg(long)]
        relevance: bool,
        #[arg(long)]
        oldest: bool,
    },
    /// Sorting mail, as a dry run: where each message would go and why. Moves nothing.
    Triage {
        /// Folders to look at (default: the Inbox of every mailbox the spam filter works in).
        #[arg(long = "folder", short = 'f')]
        folders: Vec<String>,
        /// How far back, in days.
        #[arg(long, default_value_t = 365)]
        days: i64,
        /// At most this many messages (newest first).
        #[arg(long, default_value_t = 200_000)]
        limit: usize,
        /// Messages listed per category.
        #[arg(long, default_value_t = 10)]
        show: usize,
        /// Only this category (blocked, rule, spam, priority, people, list, bulk, other).
        #[arg(long)]
        category: Option<String>,
        /// Print every reason under each message.
        #[arg(long)]
        why: bool,
        /// Every message's decision into a spreadsheet (CSV) to review.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Build the automatic whitelist from your sent mail first.
        #[arg(long)]
        whitelist: bool,
        /// Just this message, with all its reasons.
        #[arg(long)]
        id: Option<i64>,
        /// Which messages a "like this one" rule made from this message would catch (last --days).
        #[arg(long)]
        like: Option<i64>,
        /// Mark this message Spam: blacklist its sender, and catch the same mailing from other addresses.
        #[arg(long)]
        spam: Option<i64>,
        /// Mark this message Not Spam: whitelist its sender.
        #[arg(long)]
        not_spam: Option<i64>,
        /// Your primary languages, e.g. en,it (mail in other writing systems goes to Spam).
        #[arg(long, value_delimiter = ',')]
        languages: Option<Vec<String>>,
        /// Trains each mailbox's learning from what it holds (Junk / Spam vs people you write
        /// to), for the mailboxes the spam filter works in; then the dry run uses it.
        #[arg(long)]
        train: bool,
        /// Carry out what the dry run proposes (as Sort with the Spam Filter's Move does): every
        /// message looked at is sorted, 2,000 at a time; those that stay are recorded as looked at.
        #[arg(long)]
        apply: bool,
        /// Keeps your Spamhaus DQS key (from this environment variable; empty forgets it) in
        /// this computer's credential store: then new mail is checked against Spamhaus.
        #[arg(long)]
        spamhaus_key_env: Option<String>,
    },
    /// A folder's messages, newest first.
    List {
        folder: String,
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
        /// Start this many rows down (to try deep pages).
        #[arg(long, default_value_t = 0)]
        skip: u64,
    },
    /// Folders and how many messages each holds.
    Folders,
    /// One message: its headers, then its text.
    Show { id: i64 },
    /// Reservations ahead in the last year's Travel mail (flights, hotel stays), and whether
    /// the calendar has each already: a dry run, nothing changes (the download adds them).
    Travel,
    /// The address book: every contact (the words narrow it), or exported to a file.
    Contacts {
        /// Words to look for (name, address, number, company).
        query: Vec<String>,
        /// One account only (its number from `eudora-cli accounts`).
        #[arg(long)]
        account: Option<i64>,
        /// Write them to this file: vCard (.vcf) or CSV (.csv).
        #[arg(long)]
        export: Option<PathBuf>,
        /// Print them as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Signs in to a Google account (Gmail, contacts, calendars) through the browser; the
    /// download then brings its mail in.
    GoogleLogin {
        /// The Google address meant (else Google asks which).
        address: Option<String>,
    },
    /// Writes a stored message's original bytes to a file.
    Raw { id: i64, out: PathBuf },
    /// Converts an Outlook .msg file into an internet message (.eml), as Eudora's message viewer
    /// shows it (no library needed). With --json, says what's in it instead.
    MsgToEml {
        /// The .msg file.
        file: PathBuf,
        /// Where to write the .eml (default: beside the .msg, with .eml for .msg).
        out: Option<PathBuf>,
        /// Print the subject, sender, date, and attachments as JSON (nothing written).
        #[arg(long)]
        json: bool,
    },
    /// Runs one read-only SQL query on the library's database (SELECT only; for finding out
    /// why something happened). Columns are separated by tabs.
    Sql { query: String },
    /// Sizes and counts.
    Stats,
    /// Checks every stored message against its checksum, the packs and the index.
    Verify,
    /// Rebuilds the search index from the stored messages.
    Reindex,
    /// Reads the text in a picture (the system's OCR), to check it works here.
    Ocr {
        file: PathBuf,
        /// Use ocrs (the fallback for systems without OCR) instead of the system's.
        #[arg(long)]
        fallback: bool,
    },
    /// Mail statistics: received and sent, busiest days, kinds of mail, storage.
    MailStats,
    /// Shows each duplicate from its most original copy: the user's own mailbox files first,
    /// then a server's MIME, then MIME Eudora converted from an Exchange export (the download
    /// does this as copies arrive; this goes over everything).
    PreferOriginals,
    /// Changes messages' status, priority or labels (Message ▸ Change Status / Priority /
    /// Label). In Microsoft 365 mailboxes the change goes to the server (the download does it).
    Mark {
        /// Message numbers (as `eudora list` / `search` show them).
        #[arg(required = true)]
        ids: Vec<i64>,
        /// unread, read, replied, forwarded or redirected.
        #[arg(long)]
        status: Option<String>,
        /// 1 (highest) … 5 (lowest), raise, lower, or none (as the message says).
        #[arg(long)]
        priority: Option<String>,
        /// Gives them this label (Outlook category); may be given several times.
        #[arg(long)]
        label: Vec<String>,
        /// Takes this label off; may be given several times.
        #[arg(long)]
        unlabel: Vec<String>,
        /// Takes every label off.
        #[arg(long)]
        no_labels: bool,
    },
    /// Special ▸ Optimize Library: gives back the space of deleted mail, merges the search
    /// index, tidies the database (--vacuum: rewrites it too). The download must not be running.
    Optimize {
        #[arg(long)]
        vacuum: bool,
    },
    /// Removes the Exchange exports kept beside Online Archive items (the messages stay; the
    /// archive stays on Microsoft's server) and stops keeping new ones; Optimize then gives the
    /// space back. Without --yes, only says how much. The download must not be running.
    DropExports {
        #[arg(long)]
        yes: bool,
    },
    /// Linux, run as root by pkexec when Eudora updates a .deb / .rpm: checks the downloaded
    /// update in FOLDER again (signature, checksum, newer) from a copy only root can write, then
    /// installs it with dpkg / rpm. Opens no library.
    #[command(hide = true)]
    ApplyUpdate { folder: PathBuf },
    /// Special ▸ Forget Sign-ins: lists the sign-ins and passwords kept on this computer; with
    /// --yes, forgets them.
    ForgetSignIns {
        #[arg(long)]
        yes: bool,
    },
    /// File ▸ Work Offline: on, off, or (nothing) says which.
    Offline { state: Option<String> },
    /// Settings ▸ Accounts ▸ Changes on the server: which accounts keep their mail, calendar
    /// (with To Do), or contacts on this computer only. With ADDRESS and --mail / --calendar /
    /// --contacts local (kept here only) or server (Eudora's changes go to the server), changes
    /// that account's choice. Changes made while local are never sent later.
    LocalOnly {
        address: Option<String>,
        #[arg(long, value_name = "local|server")]
        mail: Option<String>,
        #[arg(long, value_name = "local|server")]
        calendar: Option<String>,
        #[arg(long, value_name = "local|server")]
        contacts: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The signatures (and each address's default), as JSON; with --set FILE, keeps those.
    Signatures {
        #[arg(long)]
        set: Option<PathBuf>,
    },
    /// Checks the spelling of some text (the system's spell checker), as JSON.
    Spell { text: String },
    /// The labels to choose from (name and color), as JSON; with --set FILE, keeps that list.
    Labels {
        #[arg(long)]
        set: Option<PathBuf>,
    },
    /// Special ▸ Empty Trash: lists each account's Trash; with --yes, empties those named by
    /// --account (all when none). Microsoft 365: deleted on the server too (Recoverable Items).
    EmptyTrash {
        /// An account's number (as `eudora-cli accounts` shows); may be given several times.
        #[arg(long)]
        account: Vec<i64>,
        #[arg(long)]
        yes: bool,
    },
    /// Synthetic Views (Receipts, Invoices, Renewals, Bank Transactions, New Accounts, 2FA
    /// codes): tags what isn't tagged yet, and says how many each holds.
    Views,
    /// Export to Classic Eudora: writes a folder classic Eudora 7 opens as its mail folder
    /// (.mbx / .toc mailboxes, descmap.pce, Filters.pce, NNdbase.txt, eudora.ini, Attach), for
    /// going back to the old program. An earlier export in that folder is brought up to date.
    /// Rules classic Eudora can't do are left out and listed in "Eudora export report.txt".
    ExportClassic {
        /// The folder (empty, or an earlier export; never inside Dropbox or OneDrive).
        dest: PathBuf,
        /// Only these accounts (their numbers from `accounts`, or addresses); default every one.
        #[arg(long, value_delimiter = ',')]
        accounts: Vec<String>,
        /// Leave the rules out.
        #[arg(long)]
        no_rules: bool,
        /// Leave the address book out.
        #[arg(long)]
        no_contacts: bool,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The classic Eudora copy kept current by the download (one way: changes made in classic
    /// Eudora don't come back). With no option, says how it's set.
    ClassicMirror {
        /// Keep a classic copy current in this folder.
        #[arg(long, conflicts_with = "off")]
        on: Option<PathBuf>,
        /// Stop keeping it current (the folder stays as it is).
        #[arg(long)]
        off: bool,
        /// Only these accounts (numbers or addresses); default every one.
        #[arg(long, value_delimiter = ',')]
        accounts: Vec<String>,
    },
    /// Classic Eudora's attachment files (kept apart from the messages): finds the ones still
    /// missing, in the classic folder and in these folders (old backup drives…), and stores
    /// them in the library.
    OldAttachments {
        /// Folders to look in too (searched with everything inside them).
        folders: Vec<PathBuf>,
    },
    /// Backs up the whole library (mail, accounts, rules, settings) into one file or parts.
    /// The download must not be running (Eudora's window pauses its own).
    Backup {
        /// The backup's file (….eudora-backup).
        dest: PathBuf,
        /// Seal it with the password in this environment variable (never typed on the command
        /// line, where it would stay in the shell's history).
        #[arg(long)]
        password_env: Option<String>,
        /// The search indexes too (a quicker restore).
        #[arg(long)]
        indexes: bool,
        /// The IMAP / SMTP passwords and the sync key too.
        #[arg(long)]
        passwords: bool,
        /// Needed with --passwords and no password: type "I agree" (anyone with the file could read them).
        #[arg(long, default_value = "")]
        agree: String,
        /// Split into parts of this size: 4GB, 700MB…
        #[arg(long)]
        part_size: Option<String>,
    },
    /// Restores a backup into a new, empty library folder (--library; else the usual place, or
    /// beside it when it holds a library already), which becomes the library Eudora opens.
    Restore {
        /// The backup (any of its parts).
        backup: PathBuf,
        #[arg(long)]
        password_env: Option<String>,
        /// When the library folder already holds a Eudora library (or a restore left half
        /// done): delete everything in it first, then restore. Other files are never deleted.
        #[arg(long)]
        replace: bool,
    },
    /// Writes a synthetic test corpus (mailboxes + manifest.json).
    GenCorpus {
        out: PathBuf,
        #[arg(long, default_value_t = 100_000)]
        messages: u64,
        #[arg(long, default_value_t = 27)]
        seed: u64,
        /// Scales attachment sizes (1.0 = realistic).
        #[arg(long, default_value_t = 1.0)]
        attachment_scale: f64,
    },
    /// Compares an imported corpus with its manifest.json, and times searches and folder pages.
    CheckCorpus { corpus: PathBuf },
    /// Any of Eudora's functions by its API method, as the owner (no permission needed; logged
    /// as "command line"): `eudora-cli call v1.mail.search '{"query": "from:ann"}'`. `eudora-cli api
    /// methods` lists them all.
    Call {
        method: String,
        /// Its parameters as JSON (default {}); "-" reads them from the standard input.
        params: Option<String>,
    },
    /// Plugins (WebAssembly; docs/PLUGINS.md).
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Looks at an IMAP account's server (read-only: nothing is changed or marked read).
    Imap {
        #[command(subcommand)]
        command: ImapCommand,
    },
    /// The local API for other programs and plugins (docs/API.md).
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
    /// Microsoft 365 / Outlook.com (read-only for now).
    Graph {
        #[command(subcommand)]
        command: GraphCommand,
    },
    /// Settings and filing sync between your computers (the project's design notes § 7).
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },
}

#[derive(Subcommand)]
enum DavCommand {
    /// Finds an address's contacts and calendars server and lists its address books and calendars (saves nothing).
    Test {
        address: String,
        /// The server, when it can't be found from the address (e.g. https://dav.example.com).
        #[arg(long, default_value = "")]
        server: String,
        /// The user name, when it isn't the address.
        #[arg(long, default_value = "")]
        user: String,
        #[arg(long)]
        password_env: Option<String>,
    },
    /// Adds (or changes) an account's contacts and calendars: found as `test` does, then saved.
    Add {
        address: String,
        #[arg(long, default_value = "")]
        server: String,
        #[arg(long, default_value = "")]
        user: String,
        #[arg(long)]
        password_env: Option<String>,
    },
    /// The accounts with a contacts and calendars server.
    List,
    /// Removes an account's contacts and calendars from this library (the server keeps them).
    Remove { address: String },
    /// Reads an account's address books and calendars (the last month and next year) now.
    Sync { address: String },
}

#[derive(Subcommand)]
enum RulesCommand {
    /// Reads classic Eudora's Filters.pce (its transfers go to folders of the account given).
    ImportClassic {
        file: PathBuf,
        /// The account the classic mailboxes were imported into (its address).
        #[arg(long)]
        account: String,
        /// Add to the rules there are instead of replacing them.
        #[arg(long)]
        add: bool,
    },
    /// The rules, in order.
    List,
    /// Writes every rule to a file (JSON), e.g. a copy before clearing them.
    Export { out: PathBuf },
    /// Removes every rule (export them first to keep a copy).
    Clear,
    /// Runs the rules (those run by hand) on a folder.
    Run {
        folder: String,
        /// Say what would happen; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum SyncCommand {
    /// Starts sync on this first computer: makes a sync key, keeps it in this computer's
    /// credential store and shows its recovery phrase once (type it on your other computers).
    Setup,
    /// Joins sync on another computer with the recovery phrase from the first.
    Join { phrase: Vec<String> },
    /// Shows the recovery phrase again (only on a computer that has the key).
    Phrase,
    /// Syncs how mail is filed now, for every account this computer has.
    Now,
    /// This computer's filing doesn't look right: asks your other computers for a full copy
    /// (an SOS), which the next one to sync answers.
    Repair,
    /// Checks an account can hold sync's data: stores a test item in its "Eudora Sync" folder
    /// (or Gmail label), reads it back and deletes it. Needs no sync key.
    Check { address: String },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Installs a plugin (.wasm); then Eudora's window asks what it may do (or answer with
    /// `eudora-cli api clients` / the window).
    Install { file: PathBuf },
    List,
    /// Runs a plugin's command on messages.
    Run {
        id: String,
        command: String,
        /// Message numbers, separated by commas.
        #[arg(long, value_delimiter = ',')]
        ids: Vec<i64>,
    },
    Remove { id: String },
}

#[derive(Subcommand)]
enum ImapCommand {
    /// The account's folders on its server, with message counts.
    Folders { address: String },
    /// A folder's newest messages on the server: UID, flags, size, subject.
    Show {
        address: String,
        /// The folder: its name on the server ("INBOX") or as Eudora shows it ("Inbox").
        folder: String,
        #[arg(short, default_value_t = 20)]
        n: usize,
    },
}

#[derive(Subcommand)]
enum ApiCommand {
    /// Serves the local API without Eudora's window (only when allowed in Settings ▸
    /// Programs and plugins, or with --allow, which turns that setting on). Ctrl+C stops it.
    Serve {
        #[arg(long)]
        allow: bool,
    },
    /// The programs and plugins allowed, and what each may do.
    Clients,
    /// Takes every permission away from a client (its number in `eudora-cli api clients`).
    Revoke { client: i64 },
    /// What programs did (newest first).
    Log {
        #[arg(long)]
        client: Option<i64>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Every method, its permission and parameters (the API reference).
    Methods,
}

#[derive(Subcommand)]
enum GraphCommand {
    /// Sign in through the browser (Microsoft's sign-in and consent pages).
    Login {
        /// Also agree to the Online Archive, shared mailboxes, sending, and moving mail and
        /// making folders on the server (work and school accounts).
        #[arg(long)]
        archive: bool,
        /// The account to sign in (its address): Microsoft's page goes to it, not to whichever
        /// account the browser is signed in to.
        #[arg(long)]
        account: Option<String>,
    },
    /// Reorganizing the Online Archive on the server: what it would move where (nothing changes),
    /// or switch it on / off. Archive mail Eudora filed elsewhere is moved into the archive folder
    /// of the same path (imported there, the original soft-deleted).
    ArchiveRefile {
        #[arg(long)]
        on: bool,
        #[arg(long)]
        off: bool,
    },
    /// The moves made in Eudora waiting to be made on the server (the download makes them).
    Changes {
        /// Try the moves given up after repeated errors once more.
        #[arg(long)]
        retry: bool,
        /// List every change waiting, with its account, folders, tries and last error.
        #[arg(long)]
        list: bool,
        /// Drop the read / label / priority changes waiting for one mailbox (its address):
        /// they stay in Eudora, the server is left as it is (a message also in another
        /// Microsoft 365 mailbox keeps its change).
        #[arg(long, value_name = "ADDRESS")]
        drop_marks: Option<String>,
    },
    /// Asks Microsoft which messages were replied to, replied to all, or forwarded (in Outlook,
    /// on a phone, anywhere): how many of each, in the Inbox or the whole mailbox (read-only;
    /// the download marks them).
    Answered {
        /// The Microsoft account (else the first one signed in).
        #[arg(long)]
        account: Option<String>,
        /// The whole mailbox, not only the Inbox.
        #[arg(long)]
        all: bool,
    },
    /// Asks Microsoft for the account's address books now: how many contacts in each, or
    /// Microsoft's answer when it refuses (read-only; the download stores them).
    Contacts {
        /// The Microsoft account (else the first one signed in).
        #[arg(long)]
        account: Option<String>,
    },
    /// Asks Microsoft for the account's calendars now: each with its events in the next 30
    /// days and the first few of them, or Microsoft's answer when it refuses (read-only).
    Calendar {
        /// The Microsoft account (else the first one signed in).
        #[arg(long)]
        account: Option<String>,
    },
    /// The Online Archive's folder tree, with item counts and sizes.
    Archive,
    /// Items that couldn't be downloaded: waiting for their next try, or given up (with why).
    Failed {
        /// Ask for all of them again at the next pass.
        #[arg(long)]
        retry: bool,
    },
    /// Checks that every item of the Online Archive is in the library: lists every folder's
    /// items on the server, says which aren't here (by kind), and tries exporting a few of them.
    ArchiveMissing {
        /// Only folders whose path starts with this ("Sent Items", "Inbox").
        #[arg(long)]
        folder: Option<String>,
        /// Missing items to try exporting per folder (to see what Exchange says).
        #[arg(long, default_value_t = 3)]
        try_export: usize,
    },
    /// Exports a few items of an archive folder (Exchange's own format) into files, for study.
    ArchiveSample {
        /// The folder path in the archive, e.g. "Old Sent Items/1999".
        folder: String,
        #[arg(long, short = 'n', default_value_t = 5)]
        count: usize,
        /// Where to write the samples (keep it out of Dropbox: it's real mail).
        #[arg(long)]
        out: PathBuf,
    },
    /// Converts exported items (.fts, from `archive-sample`) into ordinary messages (.eml) and reads them back.
    Convert {
        files: Vec<PathBuf>,
        /// Where to write the .eml files (default: next to each export).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Downloads the mailbox into the library (read-only on the server; run again to resume).
    Download {
        /// Also the Online Archive (needs `login --archive`).
        #[arg(long)]
        archive: bool,
        /// Only the Online Archive.
        #[arg(long)]
        archive_only: bool,
        /// Only folders whose path starts with this ("Inbox", "Online Archive/Old Sent Items").
        #[arg(long)]
        only: Option<String>,
        /// Requests at a time (at most 4: Exchange's limit per mailbox).
        #[arg(long, default_value_t = 4)]
        workers: usize,
        /// Hidden folders too.
        #[arg(long)]
        hidden: bool,
        /// Instead of new mail: fetch again the originals of items already here whose own bytes
        /// weren't kept (before every copy was kept).
        #[arg(long)]
        originals: bool,
        /// Also the shared mailboxes added with `add-shared`.
        #[arg(long)]
        shared: bool,
        /// Leave out the IMAP accounts (added in the viewer: Settings ▸ Accounts ▸ Add account).
        #[arg(long)]
        no_imap: bool,
        /// Only the IMAP / POP3 accounts: no Microsoft 365 mail, and no Microsoft 365 sign-in used.
        #[arg(long)]
        imap_only: bool,
        /// After downloading, keep checking for new mail every this many minutes (the archive
        /// every 6 hours); runs until stopped.
        #[arg(long)]
        watch: Option<u64>,
    },
    /// Which of these mailboxes (shared, other people's) you can open, with their Inbox sizes.
    SharedCheck {
        addresses: Vec<String>,
        /// A file with one address per line.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Adds shared mailboxes (by address) as accounts of their own, after checking you can open them.
    AddShared { addresses: Vec<String> },
    /// Finds every mailbox in your organization you can open (shared mailboxes and others').
    SharedFind,
    /// The mailbox's folder tree with message counts.
    Folders,
    /// The newest messages of a folder.
    Newest {
        #[arg(default_value = "inbox")]
        folder: String,
        #[arg(long, short = 'n', default_value_t = 10)]
        count: usize,
    },
    /// Downloads one message's original bytes and reads it with Eudora's parser.
    Fetch {
        /// The message's Graph id (from `newest`); default: the newest in the Inbox.
        id: Option<String>,
    },
    /// Forget the sign-in on this computer.
    Logout,
}

/// Prints progress on one line, a few times a second; with `--log`, also keeps a record in a
/// file: every event, and the progress line once a minute (not every update: the window shows
/// those).
struct Console {
    last: Mutex<(Instant, String)>,
    started: Instant,
    log: Option<Mutex<(std::fs::File, Instant)>>,
}

/// How often the progress line goes into the log.
const LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

impl Console {
    fn new(log: Option<&std::path::Path>) -> Result<Console> {
        let log = match log {
            Some(path) => {
                let file = std::fs::OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("can't write the log {}", path.display()))?;
                Some(Mutex::new((file, Instant::now() - LOG_EVERY)))
            }
            None => None,
        };
        Ok(Console { last: Mutex::new((Instant::now() - std::time::Duration::from_secs(1), String::new())), started: Instant::now(), log })
    }

    /// A line in the log (with the time), when there is one.
    fn log_line(&self, text: &str, progress: bool) {
        let Some(log) = &self.log else { return };
        let mut log = log.lock().unwrap();
        if progress && log.1.elapsed() < LOG_EVERY {
            return;
        }
        log.1 = Instant::now();
        let _ = writeln!(log.0, "{}  {text}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }

    fn done(&self) {
        let mut last = self.last.lock().unwrap();
        if !last.1.is_empty() {
            eprintln!();
            last.1.clear();
        }
    }
}

impl Progress for Console {
    /// An event gets a line of its own (with the time), so it stays visible in the log.
    fn event(&self, text: &str) {
        let mut last = self.last.lock().unwrap();
        eprint!("\r{:<110}\r", "");
        eprintln!("{}  {text}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
        last.1.clear();
        drop(last);
        self.log_line(text, false);
    }

    fn report(&self, done: u64, total: u64, stage: &str) {
        let mut last = self.last.lock().unwrap();
        let finished = total > 0 && done >= total;
        if last.0.elapsed().as_millis() < 250 && !finished && stage == last.1 {
            return;
        }
        let secs = self.started.elapsed().as_secs_f64();
        let line = if total > 0 && !stage.contains(&thousands(total)) {
            format!("{stage}: {} of {}  ({secs:.0} s)", thousands(done), thousands(total))
        } else {
            format!("{stage}  ({secs:.0} s)")
        };
        eprint!("\r{line:<110}");
        let _ = std::io::stderr().flush();
        *last = (Instant::now(), stage.to_string());
        drop(last);
        // (only the very end goes past the once-a-minute limit: a count that runs over its
        // total, as the checks' does when items are counted twice, would log every update)
        self.log_line(&line, !(total > 0 && done == total));
    }
}

fn library_path(cli: &Cli) -> Result<PathBuf> {
    // (else the one the window uses: the library last opened by name, else the system's place
    // for app data)
    cli.library.clone().or_else(eudora_core::library::usual_library).context("say which library: --library <folder> (or set EUDORA_LIBRARY)")
}

fn date(t: i64) -> String {
    if t == 0 {
        return "(no date)".into();
    }
    chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).collect::<String>() + "…" }
}

fn print_rows(rows: &[eudora_core::MessageRow]) {
    for r in rows {
        println!("{:>9}  {}  {:<32}  {}", r.id, date(r.date), cut(&r.from, 32), cut(&r.subject, 70));
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Eudora's folder under its old name ("Eudora 27", before 1.0.0) moved first, once
    eudora_core::library::move_from_eudora_27();
    if let Some(blocked) = eudora_core::library::folder_move_blocked() {
        for program in &blocked.programs {
            eprintln!("  {program} has a file in it open: close it, then run Eudora again.");
        }
    }
    let console = Console::new(cli.log.as_deref())?;
    let result = run(&cli, &console);
    match &result {
        Ok(()) => console.log_line("Finished.", false),
        Err(e) => {
            console.done();
            console.log_line(&format!("Stopped: {e:#}"), false);
        }
    }
    result
}

fn run(cli: &Cli, console: &Console) -> Result<()> {
    match &cli.command {
        Command::Accounts => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let accounts = library.accounts()?;
            for a in &accounts {
                let address = a.address.clone().unwrap_or_else(|| "(no address)".into());
                println!("{:>4}  {:>10}  {:<34} {:<16} {}", a.id, thousands(a.messages), address, a.kind_name, a.label.as_deref().map(|l| format!("\"{l}\"")).unwrap_or_default());
                if let Some(source) = &a.source {
                    println!("{:>17}  from {source}", "");
                }
            }
            println!("{:>4}  {:>10}  messages in all, each counted once", "", thousands(library.count(&[])?));
        }
        Command::Name { account, name } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let found = library
                .accounts()?
                .into_iter()
                .find(|a| a.id.to_string() == *account || a.address.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(account)))
                .with_context(|| format!("there's no account {account}"))?;
            library.set_account_label(found.id, Some(name))?;
            println!("{} is now shown as \"{}\".", found.address.unwrap_or_default(), if name.trim().is_empty() { "its address" } else { name.trim() });
        }
        Command::Readdress { account, address } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let old = library.set_account_address(*account, address)?;
            println!("Account {account} ({old}) is now {}.", address.trim().to_lowercase());
        }
        Command::AdoptEudora { from, root } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let who = eudora_core::import::eudora_identity(from).with_context(|| format!("{} has no eudora.ini with an address", from.display()))?;
            let source = if who.personalities.is_empty() {
                from.display().to_string()
            } else {
                format!("{} (personalities: {})", from.display(), who.personalities.join(", "))
            };
            library.adopt_imported("eudora", &who.address, &who.name, &source, root.as_deref())?;
            println!("The imported mail is now the account {} (classic Eudora).", who.address);
        }
        Command::SeedCreate { out } => {
            let started = Instant::now();
            let s = eudora_core::seed::create(&library_path(&cli)?, out, console)?;
            console.done();
            println!(
                "Wrote {}: {} messages, {} files, {:.1} GB in {:.0} min{}.",
                out.display(),
                thousands(s.messages),
                thousands(s.files),
                s.bytes as f64 / 1e9,
                started.elapsed().as_secs_f64() / 60.0,
                if s.index { "" } else { " (without the search index: it's rebuilt after restoring)" }
            );
            println!("On the other computer: eudora-cli -l <new folder> seed-restore {}", out.display());
        }
        Command::SeedRestore { seed } => {
            let target = library_path(&cli)?;
            let s = eudora_core::seed::restore(seed, &target, console)?;
            console.done();
            println!("Restored {} messages into {}.", thousands(s.messages), target.display());
            // catch the index up (and rebuild it if the seed came without), record first copies
            let library = Library::open(&target, Access::Write, console)?;
            library.close(console)?;
            console.done();
            println!("Ready. Sign in (eudora-cli graph login --archive), then download: only what's new since the seed is fetched.");
        }
        Command::Dav { command } => {
            let password_for = |address: &str, env: &Option<String>| -> Result<String> {
                match env {
                    Some(v) => std::env::var(v).with_context(|| format!("the environment variable {v} isn't set")),
                    None => eudora_core::dav::password(address).ok_or_else(|| anyhow::anyhow!("no password kept for {address}: give one with --password-env VAR")),
                }
            };
            let imap_host = |library: &Library, address: &str| -> Option<String> {
                library.imap_accounts().ok()?.into_iter().find(|c| c.address.eq_ignore_ascii_case(address)).map(|c| c.server)
            };
            match command {
                DavCommand::Test { address, server, user, password_env } | DavCommand::Add { address, server, user, password_env } => {
                    let adding = matches!(command, DavCommand::Add { .. });
                    let library = Library::open(&library_path(&cli)?, if adding { Access::Write } else { Access::Read }, console)?;
                    let password = password_for(address, password_env)?;
                    let mut config = eudora_core::dav::DavConfig { address: address.clone(), user: user.clone(), server: server.clone(), contacts: None, calendars: None };
                    let found = eudora_core::dav::discover(&config, &password, imap_host(&library, address).as_deref())?;
                    println!("Address books: {}", found.contacts.as_deref().unwrap_or("(none found)"));
                    for b in &found.books {
                        println!("  {}{}  {}", b.name, if b.writable { "" } else { " (read-only)" }, b.href);
                    }
                    println!("Calendars: {}", found.calendars.as_deref().unwrap_or("(none found)"));
                    for c in &found.calendar_list {
                        println!("  {}{}  {}", c.name, if c.writable { "" } else { " (read-only)" }, c.href);
                    }
                    if adding {
                        config.contacts = found.contacts.clone();
                        config.calendars = found.calendars.clone();
                        let pass = password_env.as_ref().map(|_| password.as_str());
                        let id = library.save_dav_account(&config, pass)?;
                        library.close(console)?;
                        println!("Saved as account {id}: its contacts and calendars are read with the next check (or `eudora-cli dav sync {address}`).");
                    }
                }
                DavCommand::List => {
                    let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
                    for c in library.dav_accounts()? {
                        println!("{}  user {}  contacts {}  calendars {}", c.address, c.user(), c.contacts.as_deref().unwrap_or("-"), c.calendars.as_deref().unwrap_or("-"));
                    }
                }
                DavCommand::Remove { address } => {
                    let library = Library::open(&library_path(&cli)?, Access::Write, console)?;
                    library.remove_dav_account(address)?;
                    library.close(console)?;
                    println!("Removed {address}'s contacts and calendars from this library (the server keeps them).");
                }
                DavCommand::Sync { address } => {
                    let library = Library::open(&library_path(&cli)?, Access::Write, console)?;
                    let config = library.dav_accounts()?.into_iter().find(|c| c.address.eq_ignore_ascii_case(address)).ok_or_else(|| anyhow::anyhow!("{address} has no contacts and calendars server here"))?;
                    let account = library.dav_account_id(&config.address)?.ok_or_else(|| anyhow::anyhow!("{address} has no account here"))?;
                    let now = eudora_core::library::now();
                    // (the changes waiting for its server, then its address books and calendars read)
                    let mut work = eudora_core::dav::plan(&library, 100)?.into_iter().find(|w| w.account == account).unwrap_or(eudora_core::dav::DavWork {
                        account,
                        config: config.clone(),
                        name: String::new(),
                        contacts: false,
                        calendars: None,
                        cards: Vec::new(),
                        events: Vec::new(),
                    });
                    work.contacts = config.contacts.is_some();
                    work.calendars = config.calendars.is_some().then_some((now - 30 * 86400, now + 365 * 86400));
                    let (cards, events) = (work.cards.len(), work.events.len());
                    let result = eudora_core::dav::carry_out(&[work]);
                    let r = eudora_core::server_changes::record_batch(&library, &result)?;
                    println!("Sent: {} of {cards} contact changes, {} of {events} event changes{}{}.", r.cards_sent, r.events_sent,
                        r.cards_send_error.as_deref().map(|e| format!(" (contacts: {e})")).unwrap_or_default(),
                        r.events_send_error.as_deref().map(|e| format!(" (events: {e})")).unwrap_or_default());
                    println!("Contacts: {} added, {} changed, {} removed{}.", r.cards.added, r.cards.changed, r.cards.removed, r.cards_error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default());
                    println!("Events: {} added, {} changed, {} removed{}.", r.events.added, r.events.changed, r.events.removed, r.events_error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default());
                    library.close(console)?;
                }
            }
        }
        Command::Rules { command } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            match command {
                RulesCommand::ImportClassic { file, account, add } => {
                    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
                    let text = eudora_core::parse::decode_8bit(&bytes);
                    let mut rules = if *add { library.rules()? } else { Vec::new() };
                    let classic = eudora_core::rules::import_classic(&text, account)?;
                    let n = classic.len();
                    rules.extend(classic);
                    library.set_rules(&rules)?;
                    println!("Read {n} classic rules ({} in all now).", rules.len());
                }
                RulesCommand::Export { out } => {
                    let rules = library.rules()?;
                    std::fs::write(out, serde_json::to_string_pretty(&rules)?)?;
                    println!("{} rules written to {}.", rules.len(), out.display());
                }
                RulesCommand::Clear => {
                    let n = library.rules()?.len();
                    library.set_rules(&[])?;
                    println!("{n} rules removed.");
                }
                RulesCommand::List => {
                    for (i, r) in library.rules()?.iter().enumerate() {
                        let when = [(r.incoming, "incoming"), (r.outgoing, "outgoing"), (r.manual, "manual")]
                            .iter()
                            .filter(|(on, _)| *on)
                            .map(|(_, w)| *w)
                            .collect::<Vec<_>>()
                            .join("+");
                        let tests: Vec<String> = r
                            .tests
                            .iter()
                            .map(|t| format!("{}{} {:?} \"{}\"", if t.unless { "unless " } else { "" }, t.field, t.verb, t.value))
                            .collect();
                        let actions: Vec<String> = r.actions.iter().map(|a| serde_json::to_string(a).unwrap_or_default()).collect();
                        println!("{:>3}. {} [{}] {}: {} -> {}", i + 1, r.name, when, if r.all { "all" } else { "any" }, tests.join(" / "), actions.join(", "));
                    }
                }
                RulesCommand::Run { folder, dry_run } => {
                    let id = library.find_folder(folder)?.with_context(|| format!("there's no folder {folder}"))?;
                    let report = library.run_rules(id, None, *dry_run, console)?;
                    console.done();
                    println!(
                        "{}: {} messages checked, {} matched; {} {}, {} copied, {} deleted.",
                        folder,
                        thousands(report.checked),
                        thousands(report.matched),
                        thousands(report.moved),
                        if *dry_run { "would move" } else { "moved" },
                        thousands(report.copied),
                        thousands(report.deleted)
                    );
                    for (name, n) in report.by_rule.iter().filter(|(_, n)| *n > 0) {
                        println!("  {:>7}  {name}", thousands(*n));
                    }
                    for (what, n) in &report.not_done {
                        println!("  not done yet: {what} ({} messages)", thousands(*n));
                    }
                    for f in &report.missing_folders {
                        println!("  folder not in the library: {f}");
                    }
                }
            }
        }
        Command::RemoveCopies { folder } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let id = library.find_folder(folder)?.with_context(|| format!("there's no folder {folder}"))?;
            let n = library.remove_folder_of_copies(id)?;
            println!("Removed {folder} ({} messages, all still in their other folders).", thousands(n));
        }
        Command::Import { paths, into, restart, account, merge, separate } => {
            let mut library = Library::open(&library_path(&cli)?, Access::Write, console)?;
            let merge_account = if *merge { Some(true) } else if *separate { Some(false) } else { None };
            let options = ImportOptions { destination: into.clone(), restart: *restart, account: account.clone(), merge_account, ..Default::default() };
            let report = match eudora_core::import(&mut library, paths, &options, console) {
                Err(e @ eudora_core::Error::AccountExists { .. }) => {
                    console.done();
                    bail!("{e}\nRun the import again with --merge (add it) or --separate (keep it apart).");
                }
                other => other?,
            };
            library.close(console)?;
            console.done();
            let c = &report.counts;
            let rate = c.seen.saturating_sub(0) as f64 / report.elapsed_secs.max(0.001);
            println!(
                "{} into {}: {} messages read, {} new, {} duplicates, {} also in another folder, {} other copies merged (same Message-ID); {} fixed, {} unreadable (kept as text). {:.0} s, {:.0} messages/s{}",
                if report.canceled { "Stopped" } else { "Imported" },
                report.destination,
                thousands(c.seen),
                thousands(c.imported),
                thousands(c.duplicates),
                thousands(c.new_locations),
                thousands(c.merged),
                thousands(c.fixed),
                thousands(c.unreadable),
                report.elapsed_secs,
                rate,
                if report.resumed_after > 0 { format!(" (resumed after {})", thousands(report.resumed_after)) } else { String::new() }
            );
            for (kind, p) in &report.problems {
                println!("  {kind}: {} (e.g. {})", thousands(p.count), cut(&p.example, 100));
            }
            for (path, why) in &report.skipped_files {
                println!("  skipped {}: {why}", path.display());
            }
        }
        Command::Search { query, limit, relevance, oldest } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let order = if *relevance { SortOrder::Relevance } else if *oldest { SortOrder::Oldest } else { SortOrder::Newest };
            let (result, rows) = library.search(query, *limit, order)?;
            print_rows(&rows);
            println!("{} matches in {:.1} ms", thousands(result.total as u64), result.took.as_secs_f64() * 1000.0);
            for note in &result.notes {
                println!("Note: {note}");
            }
            for s in &result.suggestions {
                println!("Try: {}   ({})", s.query, s.why);
            }
        }
        Command::Triage { folders, days, limit, show, category, why, csv, whitelist, id, like, spam, not_spam, languages, train, spamhaus_key_env, apply } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if let Some(var) = spamhaus_key_env {
                eudora_core::blocklists::set_key(&std::env::var(var).unwrap_or_default())?;
                println!("{}", if eudora_core::blocklists::key().is_some() { "Spamhaus DQS key kept: new mail is checked against Spamhaus." } else { "Spamhaus DQS key forgotten." });
            }
            if *train {
                for m in library.spam_filter_mailboxes()?.into_iter().filter(|m| m.on) {
                    for account in &m.accounts {
                        let started = Instant::now();
                        let t = library.bayes_bootstrap(*account, console)?;
                        console.done();
                        println!("{}: learned {} spam and {} good messages ({:.0} s)", m.shown, thousands(t.spam), thousands(t.ham), started.elapsed().as_secs_f64());
                    }
                }
            }
            if let Some(codes) = languages {
                library.set_primary_languages(codes)?;
                println!("Primary languages: {}", eudora_core::triage_decide::language_names(codes));
            }
            for (marked, done) in [(spam, true), (not_spam, false)] {
                if let Some(m) = marked {
                    let what = if done { library.mark_spam(*m)? } else { library.mark_not_spam(*m)? };
                    println!("Message {m} marked {}: {}", if done { "Spam" } else { "Not Spam" }, if what.is_empty() { "nothing to change".to_string() } else { what.join("; ") });
                }
            }
            if languages.is_some() || spam.is_some() || not_spam.is_some() {
                if id.is_none() {
                    return Ok(());
                }
            }
            if *whitelist {
                let r = library.rebuild_whitelist(&library.own_addresses()?)?;
                println!("Whitelist: {} people you wrote to, {} on mail you replied to or forwarded; {} added, {} in all",
                    thousands(r.written_to), thousands(r.replied_or_forwarded), thousands(r.added), thousands(r.total));
            }
            let reasons = |d: &eudora_core::triage_decide::Decision| {
                for s in &d.symbols {
                    println!("      {:<24} {:>5}  {}", s.rule, if s.score == 0.0 { String::new() } else { format!("{:+.1}", s.score) }, s.why);
                }
            };
            if let Some(like) = like {
                let (keys, count, sample) = library.messages_like(*like, *days, *show)?;
                println!("A rule from message {like} would look at: {}", keys.describe());
                println!("{} messages in the last {days} days match. Newest:", thousands(count));
                for (id, d, from, subject) in sample {
                    println!("  {id:>9}  {}  {}  {}", date(d), cut(&from, 30), cut(&subject, 60));
                }
                return Ok(());
            }
            let ids: Vec<i64> = if folders.is_empty() {
                library.spam_filter_inboxes()? // the Inboxes of the mailboxes the spam filter works in
            } else {
                // (a path, or a folder's number: "#123")
                folders
                    .iter()
                    .map(|f| match f.strip_prefix('#').and_then(|n| n.parse::<i64>().ok()) {
                        Some(n) => Ok(n),
                        None => library.find_folder(f)?.with_context(|| format!("there's no folder {f}")),
                    })
                    .collect::<anyhow::Result<_>>()?
            };
            if let Some(id) = id {
                let p = library.triage_one(*id)?;
                println!("{}  {}  {}", date(p.date), p.from, p.subject);
                println!("  → {}{}{}  (spam score {:.1})", p.decision.category.name(), if p.decision.priority { ", HIGH PRIORITY" } else { "" },
                    p.destination.as_deref().map(|d| format!(", would move to {d}")).unwrap_or_else(|| ", stays".into()), p.decision.spam_score);
                reasons(&p.decision);
                return Ok(());
            }
            let since = eudora_core::library::now() - days * 86400;
            let preview = library.triage_preview(&ids, since, *limit, console)?;
            if *apply {
                let chosen: Vec<eudora_core::triage_decide::Chosen> = preview.proposals.iter().map(eudora_core::triage_decide::Chosen::from).collect();
                let mut moved = 0;
                for (n, part) in chosen.chunks(2000).enumerate() {
                    moved += library.triage_apply(part)?;
                    println!("Sorted {} of {} messages: {} moved so far", thousands(((n + 1) * 2000).min(chosen.len()) as u64), thousands(chosen.len() as u64), thousands(moved));
                }
                println!("Done: {} looked at, {} moved (on the servers in the background).", thousands(preview.looked_at), thousands(moved));
                return Ok(());
            }
            println!("Looked at {} messages from the last {days} days (nothing was moved):", thousands(preview.looked_at));
            let order = ["priority", "people", "other", "list", "notification", "bulk", "rule", "blocked", "spam"];
            for c in order {
                let n = preview.counts.get(c).copied().unwrap_or(0);
                println!("  {c:<9} {:>9}", thousands(n));
            }
            for c in order {
                if category.as_deref().is_some_and(|w| !w.eq_ignore_ascii_case(c)) {
                    continue;
                }
                let these: Vec<_> = preview.proposals.iter().filter(|p| p.decision.category.name() == c).take(*show).collect();
                if these.is_empty() {
                    continue;
                }
                println!("\n{} (newest {} of {}):", c.to_uppercase(), these.len(), thousands(preview.counts.get(c).copied().unwrap_or(0)));
                for p in these {
                    println!("  {:>9}  {}  {:<30}  {}{}", p.id, date(p.date), cut(&p.from, 30), cut(&p.subject, 60),
                        p.destination.as_deref().map(|d| format!("  → {d}")).unwrap_or_default());
                    if *why {
                        reasons(&p.decision);
                    } else if let Some(s) = p.decision.symbols.first() {
                        println!("             because {} [rule: {}]", s.why, s.rule);
                    }
                }
            }
            if let Some(path) = csv {
                let q = |t: &str| format!("\"{}\"", t.replace('"', "\"\""));
                let mut out = String::from("\u{feff}id,date,from,subject,category,priority,destination,spam score,reasons\r\n");
                for p in &preview.proposals {
                    let why: Vec<&str> = p.decision.symbols.iter().map(|s| s.why.as_str()).collect();
                    out.push_str(&format!("{},{},{},{},{},{},{},{:.1},{}\r\n", p.id, q(&date(p.date)), q(&p.from), q(&p.subject), p.decision.category.name(),
                        if p.decision.priority { "yes" } else { "" }, q(p.destination.as_deref().unwrap_or("")), p.decision.spam_score, q(&why.join("; "))));
                }
                std::fs::write(path, out)?;
                println!("\nEvery decision written to {} ({} rows).", path.display(), thousands(preview.proposals.len() as u64));
            }
            println!("\nOne message in detail: eudora-cli triage --id <id>.  What a \"like this one\" rule would catch: eudora-cli triage --like <id>.");
        }
        Command::List { folder, limit, skip } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let id = library.find_folder(folder)?.with_context(|| format!("there's no folder {folder}"))?;
            let started = Instant::now();
            let cursor: Option<PageCursor> = if *skip > 0 { library.cursor_at(id, skip - 1)? } else { None };
            let t_cursor = started.elapsed();
            let started = Instant::now();
            let rows = library.list_folder(id, cursor, *limit)?;
            let t_page = started.elapsed();
            print_rows(&rows);
            println!("page of {} in {:.1} ms{}", rows.len(), t_page.as_secs_f64() * 1000.0,
                if *skip > 0 { format!(" (finding row {} took {:.1} ms; the UI keeps its place instead)", thousands(*skip), t_cursor.as_secs_f64() * 1000.0) } else { String::new() });
        }
        Command::Folders => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            for f in library.folders()? {
                println!("{:>10}  {}", thousands(f.messages), f.path);
            }
        }
        Command::Show { id } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let raw = library.raw_message(*id)?;
            let m = eudora_core::parse::parse(&raw, Default::default(), eudora_core::library::now());
            println!("From:    {}", m.from_display());
            println!("To:      {}", m.to.iter().map(|a| a.display()).collect::<Vec<_>>().join(", "));
            println!("Date:    {}", date(m.date));
            println!("Subject: {}", m.subject);
            println!("Message-ID: {}{}", m.message_id, if m.message_id_made { " (made: it had none)" } else { "" });
            for a in &m.attachments {
                println!("Attachment: {} ({}, {} bytes, {})", a.name, a.mime, a.size, a.kind.code());
            }
            for p in &m.problems {
                println!("Fixed: {}", p.detail);
            }
            println!("\n{}", m.body_text);
        }
        Command::Stats => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let s = library.stats()?;
            let mb = |b: u64| format!("{:.1} MB", b as f64 / 1e6);
            println!("Messages:      {} ({} locations in {} folders)", thousands(s.messages), thousands(s.locations), thousands(s.folders));
            println!("Mail (raw):    {}", mb(s.raw_bytes));
            println!("Packs:         {} in {} files ({:.1}x smaller{})", mb(s.pack_bytes), s.packs,
                s.raw_bytes as f64 / s.pack_bytes.max(1) as f64, s.dictionary.map(|d| format!(", dictionary {d}")).unwrap_or_default());
            println!("Database:      {}", mb(s.db_bytes));
            println!("Search index:  {} ({} entries)", mb(s.index_bytes), thousands(s.indexed_docs));
            println!("Attachments:   {} distinct in {} messages' parts", thousands(s.attachments), thousands(s.attachment_refs));
            println!("Addresses:     {}", thousands(s.addresses));
            if let (Some(a), Some(b)) = (s.oldest, s.newest) {
                println!("Dates:         {} to {}", date(a), date(b));
            }
        }
        Command::Verify => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let r = eudora_core::verify::verify(&library, console)?;
            console.done();
            println!(
                "{} of {} messages intact; {} damaged; {} packs, {} records ({} not referenced), {} torn bytes; index {} entries ({})",
                thousands(r.ok), thousands(r.messages), r.damaged_count, r.packs, thousands(r.records), r.unreferenced, r.torn_bytes,
                thousands(r.indexed), if r.index_matches { "matches" } else { "DOESN'T match" }
            );
            for (id, why) in r.damaged.iter().take(20) {
                println!("  message {id}: {why}");
            }
            if !r.passed() {
                bail!("the library has problems (see above)");
            }
        }
        Command::Backup { dest, password_env, indexes, passwords, agree, part_size } => {
            let root = library_path(&cli)?;
            let part_size = part_size.as_deref().map(parse_size).transpose()?;
            let password = password_env.as_deref().map(|v| std::env::var(v).with_context(|| format!("the environment variable {v} isn't set"))).transpose()?;
            let options = eudora_core::backup::BackupOptions { dest: dest.clone(), password, indexes: *indexes, passwords: *passwords, agreed: agree.clone(), part_size };
            let r = eudora_core::backup::back_up(&root, &options, console)?;
            console.done();
            println!("Backed up {} files ({:.1} GB){} into:", thousands(r.files), r.bytes as f64 / 1e9, if r.secrets > 0 { format!(" and {} passwords", r.secrets) } else { String::new() });
            for p in &r.parts {
                println!("  {}", p.display());
            }
        }
        Command::Restore { backup, password_env, replace } => {
            let (header, parts) = eudora_core::backup::describe(backup)?;
            println!(
                "Backup of {} ({} parts{}{}).",
                chrono::DateTime::from_timestamp(header.created, 0).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default(),
                parts.len(),
                if header.encrypted { ", with a password" } else { "" },
                if header.passwords { ", passwords included" } else { "" }
            );
            let password = password_env.as_deref().map(|v| std::env::var(v).with_context(|| format!("the environment variable {v} isn't set"))).transpose()?;
            // (the search index is built here when the backup has none; --replace needs the
            // folder named: the usual place is chosen only when it holds no library)
            if *replace && cli.library.is_none() {
                anyhow::bail!("--replace replaces the library in the folder named by --library: name it");
            }
            let (dest, r) = eudora_core::library::restore_as_library(backup, cli.library.as_deref(), password.as_deref(), *replace, console)?;
            console.done();
            println!("Restored {} files ({:.1} GB){} into {}.", thousands(r.files), r.bytes as f64 / 1e9, if r.secrets > 0 { format!(" and {} passwords", r.secrets) } else { String::new() }, dest.display());
            if r.rebuild_indexes {
                println!("Search index built.");
            }
            println!("It's now the library Eudora opens (the Start Menu shortcut and `eudora-cli` without --library).");
            println!("Microsoft 365 accounts sign in again on this computer: eudora-cli graph login --archive");
        }
        Command::ExportClassic { dest, accounts, no_rules, no_contacts, json } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let options = eudora_core::classic_export::ClassicOptions {
                accounts: account_ids(&library, accounts)?,
                folders: Vec::new(),
                no_rules: *no_rules,
                no_contacts: *no_contacts,
            };
            let r = eudora_core::classic_export::export(&library, dest, &options, console)?;
            console.done();
            if *json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print!("{}", eudora_core::classic_export::report_text(&r).replace("\r\n", "\n"));
            }
            if r.waiting.is_some() {
                std::process::exit(3);
            }
        }
        Command::ClassicMirror { on, off, accounts } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if let Some(folder) = on {
                let settings = eudora_core::classic_export::MirrorSettings {
                    on: true,
                    folder: std::path::absolute(folder).unwrap_or_else(|_| folder.clone()).display().to_string(),
                    accounts: account_ids(&library, accounts)?,
                    ..Default::default()
                };
                library.set_classic_mirror(Some(&settings))?;
                println!("The download keeps a classic Eudora copy current in {} (one way: changes made in classic Eudora don't come back).", settings.folder);
            } else if *off {
                library.set_classic_mirror(None)?;
                println!("The classic Eudora copy is no longer kept current (its folder stays as it is).");
            }
            let m = library.classic_mirror_saved()?;
            if m.on {
                println!("Kept current: {} ({})", m.folder, if m.accounts.is_empty() { "every account".to_string() } else { format!("accounts {:?}", m.accounts) });
            } else {
                println!("Not kept current{}.", if m.folder.is_empty() { String::new() } else { format!(" (last folder: {})", m.folder) });
            }
        }
        Command::OldAttachments { folders } => {
            let library = Library::open(&library_path(&cli)?, Access::Write, console)?;
            let r = library.find_old_attachments(folders, console)?;
            library.close(console)?;
            console.done();
            println!(
                "Classic attachments: {} referred to; found now {} on disk and {} in other copies of their messages ({:.1} MB stored); {} still missing ({} of them with several different files of that name: none taken).",
                thousands(r.references),
                thousands(r.found_on_disk),
                thousands(r.found_in_copies),
                r.stored_bytes as f64 / 1e6,
                thousands(r.missing),
                thousands(r.ambiguous)
            );
        }
        Command::Views => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let total = library.stats()?.messages;
            let mut done = 0u64;
            loop {
                let n = library.tag_views(50_000)?;
                if n == 0 {
                    break;
                }
                done += n;
                console.report(done.min(total), total, &format!("Sorting mail into views: {} of {}", thousands(done.min(total)), thousands(total)));
            }
            // Red Alert candidates: their headers read, confirmed or not
            let mut checked = 0u64;
            loop {
                let n = library.confirm_alerts(2000)?;
                if n == 0 {
                    break;
                }
                checked += n;
                console.report(0, 0, &format!("Checking possible Red Alerts: {} so far", thousands(checked)));
            }
            // payments and receipts: read for amounts, then linked
            let mut read = 0u64;
            loop {
                let n = library.link_payments(2000)?;
                if n == 0 {
                    break;
                }
                read += n;
                console.report(0, 0, &format!("Reading payments and receipts for amounts: {} so far", thousands(read)));
            }
            console.done();
            let links: i64 = library.connection().query_row("SELECT COUNT(*) FROM links", [], |r| r.get(0))?;
            println!("Payments linked to their receipts: {}", thousands(links as u64));
            for v in library.views(&[])? {
                println!("  {:<18} {:>9}", v.name, thousands(v.messages));
            }
        }
        Command::Mark { ids, status, priority, label, unlabel, no_labels } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if let Some(s) = status {
                let status = eudora_core::marks::Status::parse(s).with_context(|| format!("there's no status \"{s}\" (unread, read, replied, forwarded, redirected)"))?;
                println!("Status {s}: {} changed.", library.set_status(ids, status)?);
            }
            if let Some(p) = priority {
                let n = match p.to_ascii_lowercase().as_str() {
                    "raise" => library.shift_priority(ids, -1)?,
                    "lower" => library.shift_priority(ids, 1)?,
                    "none" | "0" => library.set_priority(ids, None)?,
                    level => library.set_priority(ids, Some(level.parse().with_context(|| format!("a priority is 1 to 5, raise, lower or none, not \"{p}\""))?))?,
                };
                println!("Priority {p}: {n} changed.");
            }
            if *no_labels {
                println!("Labels taken off: {} changed.", library.clear_labels(ids)?);
            }
            for name in label {
                println!("Label {name}: given to {}.", library.set_label(ids, name, true)?);
            }
            for name in unlabel {
                println!("Label {name}: taken off {}.", library.set_label(ids, name, false)?);
            }
        }
        Command::Call { method, params } => {
            let text = match params.as_deref() {
                None => "{}".to_string(),
                Some("-") => {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                    s
                }
                Some(p) => p.to_string(),
            };
            // (Windows PowerShell 5.1 starts what it pipes to a program with a byte-order mark)
            let params: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).context("the parameters aren't JSON")?;
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            match library.api_owner_call(&eudora_core::api::NoHost, method, &params) {
                Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
                Err(r) => bail!("{} (code {})", r.message, r.code),
            }
        }
        Command::Plugin { command } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            match command {
                PluginCommand::Install { file } => {
                    let m = library.plugin_install(file)?;
                    println!("Installed {} {} from {} (not signed: Eudora can't check who made it).", m.name, m.version, m.publisher);
                    println!("It asks to: {}", m.permissions.iter().map(|g| g.permission.plainly()).collect::<Vec<_>>().join("; "));
                    println!("Eudora's window asks you what it may do (it can do nothing until then).");
                }
                PluginCommand::List => {
                    for p in library.plugins()? {
                        let m = &p.manifest;
                        println!("{}  {} {} from {}{}{}", m.id, m.name, m.version, m.publisher, if p.enabled { "" } else { " (off)" }, if p.granted { "" } else { " (waiting for permission)" });
                        for c in m.commands.iter().chain(&m.rule_actions) {
                            println!("    {}: {}", c.id, c.title);
                        }
                    }
                }
                PluginCommand::Run { id, command, ids } => {
                    let out = library.plugin_run(id, command, &serde_json::json!({ "ids": ids }))?;
                    println!("{}", serde_json::to_string_pretty(&out)?);
                }
                PluginCommand::Remove { id } => {
                    library.plugin_remove(id)?;
                    println!("Removed {id}.");
                }
            }
        }
        Command::Imap { command } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let address = match command {
                ImapCommand::Folders { address } | ImapCommand::Show { address, .. } => address,
            };
            let config = library
                .imap_accounts()?
                .into_iter()
                .find(|c| c.address.eq_ignore_ascii_case(address))
                .ok_or_else(|| eudora_core::Error::Other(format!("no IMAP account {address} in this library")))?;
            match command {
                ImapCommand::Folders { .. } => {
                    for f in eudora_core::imap_sync::server_folders(&config)? {
                        println!("{:>8}  {}  ({})", thousands(f.messages as u64), f.path, f.server_name);
                    }
                }
                ImapCommand::Show { folder, n, .. } => {
                    let (validity, messages) = eudora_core::imap_sync::peek(&config, folder, *n)?;
                    println!("{folder}: UIDVALIDITY {validity}, newest {}", messages.len());
                    for m in messages {
                        println!("{:>7}  {:>9}  {:<34}  {}  {}", m.uid, thousands(m.size as u64), m.flags.join(" "), m.subject, m.message_id);
                    }
                }
            }
        }
        Command::Api { command: ApiCommand::Methods } => {
            for m in eudora_core::api::all_methods() {
                let p = m.permission.map(|p| serde_json::to_value(p).unwrap_or_default().as_str().unwrap_or_default().to_string()).unwrap_or_else(|| "(anyone)".into());
                println!("{}  [{p}{}]\n    {}\n    params {}", m.name, if m.write { ", changes" } else { "" }, m.summary, m.params);
            }
        }
        Command::Api { command } => {
            let root = library_path(&cli)?;
            let library = Library::open(&root, Access::Read, console)?;
            match command {
                ApiCommand::Serve { allow } => {
                    let mut settings = library.api_settings()?;
                    if *allow && !settings.without_window {
                        settings.without_window = true;
                        library.set_api_settings(&settings)?;
                    }
                    if !settings.without_window {
                        bail!("the API is off while Eudora's window is closed: allow it in Settings ▸ Programs and plugins, or run `eudora-cli api serve --allow`");
                    }
                    let library = std::sync::Arc::new(std::sync::Mutex::new(library));
                    let limits = std::sync::Arc::new(eudora_core::api::Limits::default());
                    let handler: eudora_core::api_server::Handler = std::sync::Arc::new(move |token, method, params| {
                        let library = library.lock().unwrap();
                        library.api_call(&limits, &eudora_core::api::NoHost, token, method, params)
                    });
                    println!("Serving the API on {} (this computer, your account only). Ctrl+C stops it.", eudora_core::api_server::address(&root));
                    eudora_core::api_server::serve(&root, handler, std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))?;
                }
                ApiCommand::Clients => {
                    for c in library.api_clients()? {
                        let state = if c.revoked.is_some() { "revoked" } else if c.granted { "allowed" } else { "asking" };
                        println!("{:>3}  {} ({} from {}): {state}; log {:?}; sends {}", c.id, c.name, c.kind, c.publisher, c.log_level, if c.send_without_asking { "without asking" } else { "confirmed" });
                        for g in &c.grants {
                            println!("       {}", serde_json::to_string(g)?);
                        }
                    }
                }
                ApiCommand::Revoke { client } => {
                    library.api_revoke(*client)?;
                    println!("Client {client} revoked.");
                }
                ApiCommand::Log { client, limit } => {
                    for l in library.api_log(*client, *limit)? {
                        let at = chrono::DateTime::from_timestamp(l.at, 0).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default();
                        println!("{at}  {:<20} {:<22} {}{}", l.client_name.unwrap_or_else(|| "Eudora".into()), l.method, if l.allowed { "" } else { "REFUSED " }, l.detail);
                    }
                }
                ApiCommand::Methods => {
                    for m in eudora_core::api::all_methods() {
                        let p = m.permission.map(|p| serde_json::to_value(p).unwrap_or_default().as_str().unwrap_or_default().to_string()).unwrap_or_else(|| "(anyone)".into());
                        println!("{}  [{p}{}]\n    {}\n    params {}", m.name, if m.write { ", changes" } else { "" }, m.summary, m.params);
                    }
                }
            }
        }
        Command::ApplyUpdate { folder } => {
            let version = eudora_core::update::apply_update(folder)?;
            println!("installed {version}");
        }
        Command::DropExports { yes } => {
            let path = library_path(&cli)?;
            if !*yes {
                let library = Library::open(&path, Access::Read, console)?;
                let (n, bytes) = library.exchange_exports()?;
                println!("{n} Exchange exports, {:.1} GB as stored. Run again with --yes to remove them.", bytes as f64 / 1e9);
            } else {
                let library = Library::open(&path, Access::Write, console)?;
                let (n, bytes) = library.drop_exchange_exports()?;
                library.close(console)?;
                println!("Removed {n} Exchange exports ({:.1} GB as stored); new archive items won't keep one. Run `eudora-cli optimize` to give the space back.", bytes as f64 / 1e9);
            }
        }
        Command::Optimize { vacuum } => {
            let library = Library::open(&library_path(&cli)?, Access::Write, console)?;
            let r = library.optimize(*vacuum, console)?;
            library.close(console)?;
            println!(
                "Optimized: {:.0} MB given back ({} packs rewritten, {} removed, {} messages moved, {} attachment files let go); search index merged from {} parts; database {:.0} MB → {:.0} MB.",
                r.bytes_freed as f64 / 1e6,
                r.packs_rewritten,
                r.packs_removed,
                r.records_moved,
                r.attachment_files_dropped,
                r.index_segments,
                r.database_before as f64 / 1e6,
                r.database_after as f64 / 1e6
            );
        }
        Command::ForgetSignIns { yes } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if *yes {
                for what in library.forget_sign_ins()? {
                    println!("Forgotten: {what}");
                }
            } else {
                for s in library.sign_ins()? {
                    println!("{}", s.what);
                }
                println!("(add --yes to forget them on this computer)");
            }
        }
        Command::Offline { state } => {
            let root = library_path(&cli)?;
            match state.as_deref() {
                Some("on") => eudora_core::graph_sync::set_offline(&root, true)?,
                Some("off") => eudora_core::graph_sync::set_offline(&root, false)?,
                Some(other) => anyhow::bail!("on or off, not \"{other}\""),
                None => {}
            }
            println!("{}", if eudora_core::graph_sync::is_offline(&root) { "Working offline." } else { "Online." });
        }
        Command::LocalOnly { address, mail, calendar, contacts, json } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let side = |v: &Option<String>| -> anyhow::Result<Option<bool>> {
                match v.as_deref() {
                    None => Ok(None),
                    Some("local") => Ok(Some(true)),
                    Some("server") => Ok(Some(false)),
                    Some(other) => anyhow::bail!("local or server, not \"{other}\""),
                }
            };
            let (m, c, k) = (side(mail)?, side(calendar)?, side(contacts)?);
            if m.is_some() || c.is_some() || k.is_some() {
                let Some(address) = address else { anyhow::bail!("name the account's address to change its choice") };
                let mut choice = library.local_only(address)?;
                choice.mail = m.unwrap_or(choice.mail);
                choice.calendar = c.unwrap_or(choice.calendar);
                choice.contacts = k.unwrap_or(choice.contacts);
                let dropped = library.set_local_only(address, choice)?;
                if dropped > 0 && !*json {
                    println!("{dropped} changes that were waiting for the server were dropped (they stay in Eudora).");
                }
            }
            let rows: Vec<_> = library.local_only_list()?.into_iter().filter(|r| address.as_deref().is_none_or(|a| r.address.eq_ignore_ascii_case(a))).collect();
            if *json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for r in &rows {
                    let said: Vec<String> = r
                        .services
                        .iter()
                        .map(|s| format!("{s}: {}", if r.local.of(eudora_core::local_only::Service::parse(s).unwrap_or(eudora_core::local_only::Service::Mail)) { "local only" } else { "to the server" }))
                        .collect();
                    println!("{} ({}): {}", r.address, r.kind, said.join(", "));
                }
            }
        }
        Command::Signatures { set } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if let Some(file) = set {
                library.set_signatures(&serde_json::from_slice(&std::fs::read(file)?)?)?;
            }
            println!("{}", serde_json::to_string_pretty(&library.signatures()?)?);
        }
        Command::Spell { text } => {
            println!("{}", serde_json::to_string_pretty(&eudora_core::spell::check(std::slice::from_ref(text))?)?);
        }
        Command::Labels { set } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if let Some(file) = set {
                let list: Vec<eudora_core::marks::LabelDef> = serde_json::from_slice(&std::fs::read(file)?)?;
                library.set_label_list(&list, &[])?;
            }
            println!("{}", serde_json::to_string_pretty(&library.label_list()?)?);
        }
        Command::EmptyTrash { account, yes } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let summary = library.trash_summary()?;
            for t in &summary {
                println!(
                    "{:>3}  {} › {}: {} messages{}{}",
                    t.account,
                    t.shown,
                    t.path,
                    thousands(t.messages),
                    if t.on_server { " (deleted on Microsoft 365 too)" } else { "" },
                    if t.archive_kept > 0 { format!("; {} in the Online Archive kept", thousands(t.archive_kept)) } else { String::new() }
                );
            }
            if *yes {
                let chosen: Vec<i64> = if account.is_empty() { summary.iter().map(|t| t.account).collect() } else { account.clone() };
                println!("Emptied: {} messages.", thousands(library.empty_trash(&chosen)?));
            } else if !summary.is_empty() {
                println!("Nothing deleted: add --yes (and --account N to choose) to empty them.");
            }
        }
        Command::PreferOriginals => {
            let mut library = Library::open(&library_path(&cli)?, Access::Write, console)?;
            let done = library.prefer_originals_all(console)?;
            println!("Messages with several copies: {}; now shown from a more original copy: {}.", thousands(done.looked), thousands(done.swapped));
        }
        Command::MailStats => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let started = Instant::now();
            let s = library.mail_stats()?;
            let gb = |b: u64| format!("{:.1} GB", b as f64 / 1e9);
            println!("{} messages ({:.1} s to count)", thousands(s.messages), started.elapsed().as_secs_f64());
            for p in &s.periods {
                println!("  {:<15} received {:>10}   sent {:>9}", p.label, thousands(p.received), thousands(p.sent));
            }
            println!("Busiest days:");
            for (d, n) in &s.busiest_days {
                println!("  {d}  {}", thousands(*n));
            }
            let days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            println!("Last 12 months by weekday: {}", days.iter().zip(&s.weekdays).map(|(d, n)| format!("{d} {}", thousands(*n))).collect::<Vec<_>>().join(", "));
            if s.sorted > 0 {
                println!("Kinds of mail (the {} messages the spam filter sorted):", thousands(s.sorted));
                for (k, n) in &s.kinds {
                    println!("  {k:<15} {:>9}  {:>5.1}%", thousands(*n), *n as f64 * 100.0 / s.sorted as f64);
                }
            }
            let st = &s.storage;
            println!(
                "Storage: messages {} uncompressed in {} of packs ({:.1}x), other originals {}; database {}, search index {}, header search {}",
                gb(st.messages_uncompressed), gb(st.packs), st.messages_uncompressed as f64 / st.packs.max(1) as f64, gb(st.copies_uncompressed), gb(st.database), gb(st.search_index), gb(st.header_index)
            );
        }
        Command::Ocr { file, fallback } => {
            let bytes = std::fs::read(file)?;
            println!("System OCR here: {}", if eudora_core::ocr::system_available() { "yes" } else { "no" });
            let text = if *fallback { eudora_core::ocr::fallback_text(&bytes) } else { eudora_core::ocr::picture_text(&bytes) };
            match text {
                Some(text) => println!("{text}"),
                None => println!("(no text read)"),
            }
        }
        Command::Reindex => {
            let library = Library::open(&library_path(&cli)?, Access::Write, console)?;
            library.rebuild_index(console)?;
            library.close(console)?;
            console.done();
            println!("Search index rebuilt.");
        }
        Command::GenCorpus { out, messages, seed, attachment_scale } => {
            let opts = eudora_testkit::Options { messages: *messages, seed: *seed, attachment_scale: *attachment_scale };
            let started = Instant::now();
            let m = eudora_testkit::generate(out, &opts, &|done, total| console.report(done, total, "Writing message"))?;
            console.done();
            println!(
                "{} messages ({:.1} GB) in {:.0} s: {} distinct, {} duplicates, {} copies in other folders, {} Message-ID variants. Manifest: {}",
                thousands(m.written), m.bytes as f64 / 1e9, started.elapsed().as_secs_f64(), thousands(m.unique), thousands(m.duplicates),
                thousands(m.copies), thousands(m.id_variants), out.join("manifest.json").display()
            );
        }
        Command::CheckCorpus { corpus } => check_corpus(&cli, corpus, console)?,
        Command::Sql { query } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            for row in library.query_text(query)? {
                println!("{}", row.join("\t"));
            }
        }
        Command::GoogleLogin { address } => {
            let client = eudora_core::google::GoogleClient::sign_in(&|url| {
                println!("Opening Google's sign-in in your browser…
If it doesn't open, go to:
{url}");
                let _ = eudora_core::graph::open_browser(url);
            }, address.as_deref())?;
            println!("Signed in as {}. The download brings its mail in at its next check.", client.account);
        }
        Command::Travel => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let found = library.travel_preview()?;
            for (f, id, subject, has) in &found {
                let when = chrono::DateTime::from_timestamp(f.start, 0).map(|t| t.with_timezone(&chrono::Local).format("%a %b %-d %Y %-I:%M %p").to_string()).unwrap_or_default();
                let to = chrono::DateTime::from_timestamp(f.end, 0).map(|t| t.with_timezone(&chrono::Local).format("%a %b %-d %-I:%M %p").to_string()).unwrap_or_default();
                println!("{} {} → {to}{}  {}", if *has { "[already on the calendar]" } else if eudora_core::travel::adds_itself(&f.kind) { "[would add]" } else { "[would ask]" }, when, if f.end_guessed { " (arrival unknown)" } else { "" }, f.title);
                println!("    {}", f.details.replace('\n', " · "));
                println!("    from message {id}: {subject}");
                // what its calendar event gets: the email's tickets and passes
                for t in library.ticket_files(*id).unwrap_or_default() {
                    println!("    ticket: {} ({})", t["name"].as_str().unwrap_or("?"), if t["url"].is_string() { "picture on the web" } else { t["contentType"].as_str().unwrap_or("?") });
                }
            }
            println!("{} reservations ahead.", found.len());
        }
        Command::Contacts { query, account, export, json } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if let Some(path) = export {
                let n = library.export_cards(path, *account)?;
                println!("Wrote {} contacts to {}", thousands(n), path.display());
            } else {
                let rows = library.cards(&query.join(" "), *account)?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    for r in &rows {
                        let emails = r.card.emails.iter().map(|e| e.value.as_str()).collect::<Vec<_>>().join(", ");
                        let phones = r.card.phones.iter().map(|p| p.value.as_str()).collect::<Vec<_>>().join(", ");
                        println!("{:>6}  {:<32}  {:<40}  {:<20}  {} › {}", r.id, r.shown, emails, phones, r.account_address, r.book);
                    }
                    println!("{} contacts", thousands(rows.len() as u64));
                }
            }
        }
        Command::MsgToEml { file, out, json } => {
            let bytes = std::fs::read(file).map_err(|e| eudora_core::Error::Other(format!("{}: {e}", file.display())))?;
            let eml = eudora_core::msg_read::to_eml(&bytes)?;
            if *json {
                let d = eudora_core::parse::display(&eml, eudora_core::library::now());
                let p = d.parsed;
                let people = |list: &[eudora_core::parse::Addr]| list.iter().map(|a| serde_json::json!({ "name": a.name, "address": a.address })).collect::<Vec<_>>();
                let attachments: Vec<_> = p.attachments.iter().map(|a| serde_json::json!({ "name": a.name, "type": a.mime, "size": a.size })).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "subject": p.subject, "from": people(&p.from), "to": people(&p.to), "cc": people(&p.cc),
                        "date": p.date, "message_id": p.message_id, "html": d.html.is_some(), "attachments": attachments, "eml_bytes": eml.len(),
                    }))?
                );
            } else {
                let out = out.clone().unwrap_or_else(|| file.with_extension("eml"));
                std::fs::write(&out, &eml)?;
                eudora_core::saving::mark_downloaded(&out);
                println!("Wrote {} bytes to {}", thousands(eml.len() as u64), out.display());
            }
        }
        Command::Raw { id, out } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let raw = library.raw_message(*id)?;
            std::fs::write(out, &raw)?;
            println!("Wrote {} bytes to {}", thousands(raw.len() as u64), out.display());
        }
        Command::Graph { command: GraphCommand::AddShared { addresses } } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let client = GraphClient::resume(None, eudora_core::graph::ALL_SCOPES)?;
            for address in addresses {
                let mut c = client.worker();
                c.set_mailbox(Some(address.clone()));
                match c.mailbox_access() {
                    Ok(inbox) => {
                        library.add_shared_mailbox(address)?;
                        println!("Added {address} (its Inbox holds {} messages); the next download with --shared brings it in.", thousands(inbox));
                    }
                    Err(e) => println!("{address}: not added, you can't open it ({e})"),
                }
            }
        }
        Command::Graph { command: GraphCommand::Failed { retry } } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if *retry {
                let n = eudora_core::graph_sync::retry_failed(&library)?;
                println!("{} items will be asked for again at the next pass.", thousands(n as u64));
                return Ok(());
            }
            let mut items: Vec<_> = eudora_core::graph_sync::failed_items(&library)?.into_iter().collect();
            items.sort_by(|a, b| a.1.path.cmp(&b.1.path).then(a.1.next.cmp(&b.1.next)));
            let given_up = items.iter().filter(|(_, f)| f.given_up()).count();
            println!("{} items couldn't be downloaded: {} waiting for their next try, {} given up.", thousands(items.len() as u64), thousands((items.len() - given_up) as u64), thousands(given_up as u64));
            for (id, f) in &items {
                let when = if f.given_up() { "given up".to_string() } else { format!("next try {}", date(f.next)) };
                println!("  {}  {} tries, {when}  {}\n      {}", f.path, f.tries, cut(id, 24), cut(&f.error, 160));
            }
        }
        Command::Graph { command: GraphCommand::ArchiveMissing { folder, try_export } } => {
            // every archive item the library knows, in any folder (moves keep their record)
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            let known: std::collections::HashSet<String> = library
                .query_text("SELECT si.server_id FROM server_items si JOIN folders f ON f.id = si.folder WHERE f.path LIKE 'Online Archive/%'")?
                .into_iter()
                .filter_map(|r| r.into_iter().next())
                .collect();
            drop(library);
            println!("{} archive items in the library", thousands(known.len() as u64));
            let mut client = GraphClient::resume(None, eudora_core::graph::ARCHIVE_SCOPES)?;
            let (_primary, archive) = client.mailbox_ids()?;
            let archive = archive.context("this account has no Online Archive")?;
            let folders = client.mailbox_folders(&archive, &|_| {})?;
            let mut path: Vec<String> = Vec::new();
            let (mut all_server, mut all_listed, mut all_unreadable, mut all_missing) = (0u64, 0u64, 0u64, 0u64);
            let mut kinds_missing: std::collections::BTreeMap<String, u64> = Default::default();
            for f in &folders {
                path.truncate(f.depth);
                path.push(f.display_name.replace('/', "-"));
                let p = path.join("/");
                if folder.as_deref().is_some_and(|w| !p.to_lowercase().starts_with(&w.to_lowercase())) || f.total_item_count == 0 {
                    continue;
                }
                let owner = GraphClient::folder_owner(&archive, f);
                // the raw pages: an item Eudora's reading would skip is counted, not lost
                let mut next = Some(format!("{owner}/folders/{}/items?$top=1000", f.id));
                let (mut listed, mut unreadable) = (0u64, 0u64);
                let mut missing: Vec<(String, String)> = Vec::new();
                while let Some(url) = next {
                    let page = match client.get_json(&url) {
                        Ok(page) => page,
                        Err(e) => {
                            println!("  {p}: couldn't list ({e})");
                            break;
                        }
                    };
                    for v in page["value"].as_array().into_iter().flatten() {
                        listed += 1;
                        let Some(id) = v["id"].as_str() else {
                            unreadable += 1;
                            continue;
                        };
                        if serde_json::from_value::<eudora_core::graph::MailboxItem>(v.clone()).is_err() {
                            unreadable += 1;
                        }
                        if !known.contains(id) {
                            missing.push((id.to_string(), v["type"].as_str().unwrap_or("?").to_string()));
                        }
                    }
                    next = page["@odata.nextLink"].as_str().map(str::to_string);
                }
                all_server += f.total_item_count;
                all_listed += listed;
                all_unreadable += unreadable;
                all_missing += missing.len() as u64;
                if missing.is_empty() && listed == f.total_item_count && unreadable == 0 {
                    continue;
                }
                let mut kinds: std::collections::BTreeMap<&str, u64> = Default::default();
                for (_, k) in &missing {
                    *kinds.entry(k.as_str()).or_default() += 1;
                    *kinds_missing.entry(k.clone()).or_default() += 1;
                }
                let kinds: Vec<String> = kinds.iter().map(|(k, n)| format!("{k} {}", thousands(*n))).collect();
                println!(
                    "{p}: server says {}, listed {}, unreadable {}, not in the library {} ({})",
                    thousands(f.total_item_count), thousands(listed), unreadable, thousands(missing.len() as u64), kinds.join(", ")
                );
                let mail: Vec<String> = missing.iter().filter(|(_, k)| k.starts_with("IPM.Note") || k.starts_with("IPM.Schedule") || k == "?").take(*try_export).map(|(id, _)| id.clone()).collect();
                if !mail.is_empty() {
                    match client.export_items(&owner, &mail) {
                        Ok(items) => {
                            for i in items {
                                match (&i.data, &i.error) {
                                    (Some(d), _) => println!("    export ok ({} bytes): it can be downloaded", d.len()),
                                    (None, e) => println!("    export failed: {}", e.as_deref().unwrap_or("no data")),
                                }
                            }
                        }
                        Err(e) => println!("    export failed: {e}"),
                    }
                }
            }
            println!(
                "\nServer counts {}, listed {}, unreadable {}, not in the library {}",
                thousands(all_server), thousands(all_listed), thousands(all_unreadable), thousands(all_missing)
            );
            for (k, n) in &kinds_missing {
                println!("{:>10}  {k}", thousands(*n));
            }
        }
        Command::Graph { command: GraphCommand::ArchiveRefile { on, off } } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if *on || *off {
                library.set_setting(eudora_core::archive_moves::SETTING, if *on { "on" } else { "off" })?;
            }
            let p = library.archive_refile_preview()?;
            println!("{} Online Archive messages are filed outside the archive in Eudora.", thousands(p.messages));
            println!("{} go back to their archive folder (filed into the Inbox: they stay where they are on the server).", thousands(p.put_back));
            for (to, n) in &p.to {
                println!("{:>8}  → Online Archive/{to}", thousands(*n));
            }
            let state = library.setting(eudora_core::archive_moves::SETTING)?.unwrap_or_default();
            println!("Reorganizing on the server: {}", if state == "on" { "on (the download moves them)" } else { "off (eudora-cli graph archive-refile --on starts it)" });
        }
        Command::Graph { command: GraphCommand::Changes { retry, list, drop_marks } } => {
            let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
            if let Some(address) = drop_marks {
                let n = eudora_core::server_changes::drop_marks(&library, address)?;
                println!("Dropped {} changes waiting for {address}: they stay in Eudora; the server is left as it is.", thousands(n));
            }
            if *retry {
                let n = eudora_core::server_changes::retry_given_up(&library)?;
                println!("{} moves will be tried again by the download.", thousands(n));
            }
            if *list {
                for line in eudora_core::server_changes::waiting_list(&library)? {
                    println!("{line}");
                }
            }
            let (waiting, given_up, last) = eudora_core::server_changes::waiting(&library)?;
            println!("Moves waiting for the server: {}", thousands(waiting));
            let (marks, marks_given_up) = eudora_core::server_changes::waiting_marks(&library)?;
            println!("Status, label and delete changes waiting: {}", thousands(marks));
            if marks_given_up > 0 {
                println!("Of those given up after repeated errors: {}", thousands(marks_given_up));
            }
            if given_up > 0 {
                println!("Given up after repeated errors: {} (eudora-cli graph changes --retry tries them again)", thousands(given_up));
            }
            if let Some(last) = last {
                println!("Last error: {last}");
            }
        }
        Command::Graph { command: GraphCommand::Download { archive, archive_only, only, workers, hidden, originals, shared, no_imap, imap_only, watch } } => {
            let mut library = Library::open(&library_path(&cli)?, Access::Write, console)?;
            let scopes = if *shared {
                eudora_core::graph::ALL_SCOPES
            } else if *archive || *archive_only {
                eudora_core::graph::ARCHIVE_SCOPES
            } else {
                eudora_core::graph::SCOPES
            };
            // with permission to change mail on the server when the user gave it (moves made
            // in Eudora are then made there too), else read-only. Without a Microsoft 365
            // sign-in (an IMAP / POP3-only setup, or not signed in yet) the mail servers' accounts
            // still download; with neither there's nothing to do. (The loop is the core's,
            // `watch`, shared with the phone apps, which run it in a thread.)
            let options = eudora_core::graph_sync::SyncOptions {
                primary: !*archive_only && !*imap_only,
                archive: (*archive || *archive_only) && !*imap_only,
                only: only.clone(),
                workers: *workers,
                hidden: *hidden,
                originals: *originals,
                shared: *shared && !*imap_only,
                imap: (!*no_imap && !*archive_only) || *imap_only,
                // (Google accounts with the mail servers: not with --imap-only)
                google: !*no_imap && !*archive_only && !*imap_only,
            };
            let w = eudora_core::watch::Watch { options, imap_only: *imap_only, scopes, every: *watch };
            let mut clients = eudora_core::watch::sign_ins(&library, &w, console)?;
            let result = eudora_core::watch::download_all(&mut library, &mut clients, &w.options, console);
            if let (Some(_), Ok(first)) = (watch, &result) {
                console.done();
                first.iter().for_each(|r| print_download(r, console));
                // keep checking for new mail (an account added during the pass: the next one
                // starts at once); runs until stopped
                let at_once = first.iter().any(|r| r.restart);
                eudora_core::watch::keep_up(&mut library, &mut clients, &w, at_once, console, &|done| report_sync(done, console))?;
            }
            library.close(console)?;
            console.done();
            result?.iter().for_each(|r| print_download(r, console));
        }
        Command::Graph { command } => graph(command)?,
        Command::Sync { command } => {
            use eudora_core::sync_envelope::SyncKey;
            match command {
                SyncCommand::Setup => {
                    if let Some(key) = SyncKey::load() {
                        anyhow::bail!("this computer already has a sync key ({}); `eudora-cli sync phrase` shows its phrase", key.id());
                    }
                    let key = SyncKey::generate()?;
                    key.store()?;
                    println!("Sync is set up on this computer (key {}).\n", key.id());
                    println!("Your recovery phrase (type it on your other computers with `eudora-cli sync join`, and keep a copy somewhere safe):\n\n    {}\n", key.phrase());
                }
                SyncCommand::Join { phrase } => {
                    let key = SyncKey::from_phrase(&phrase.join(" "))?;
                    key.store()?;
                    println!("This computer now syncs with key {}.", key.id());
                }
                SyncCommand::Check { address } => {
                    let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
                    let mut client = GraphClient::resume_best(None, eudora_core::graph::ALL_SCOPES).ok();
                    println!("{}", eudora_core::sync_run::check_store(&library, client.as_mut(), address)?);
                }
                SyncCommand::Phrase => match SyncKey::load() {
                    Some(key) => println!("{}", key.phrase()),
                    None => println!("This computer has no sync key yet: `eudora-cli sync setup` (first computer) or `eudora-cli sync join PHRASE`."),
                },
                SyncCommand::Now | SyncCommand::Repair => {
                    let library = Library::open(&library_path(&cli)?, Access::Read, console)?;
                    // (a Microsoft 365 sign-in when there is one; Gmail and IMAP accounts sync without)
                    let mut client = GraphClient::resume_best(None, eudora_core::graph::ALL_SCOPES).ok();
                    let repair = matches!(command, SyncCommand::Repair);
                    report_sync(&eudora_core::sync_run::sync_all(&library, client.as_mut(), repair)?, console);
                    if SyncKey::load().is_none() {
                        println!("Sync isn't set up on this computer: `eudora-cli sync setup` or `eudora-cli sync join PHRASE`.");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Says what filing sync did, one line per account that had something to say.
fn report_sync(all: &eudora_core::sync_run::SyncAll, console: &Console) {
    for (account, result) in &all.settings {
        match result {
            Ok(r) if r.changed > 0 || r.stored => console.event(&format!(
                "Settings sync (through {account}): {} changed here{}.",
                thousands(r.changed as u64),
                if r.stored { ", this computer's sent" } else { "" }
            )),
            Ok(r) if r.asked => console.event(&format!("Settings sync (through {account}): asked your other computers for a full copy (SOS).")),
            Ok(_) => {}
            Err(e) => console.event(&format!("Settings sync (through {account}): didn't run ({e}); trying again later.")),
        }
    }
    let done = &all.filing;
    for (account, result) in done {
        match result {
            Ok(r) if r.sent + r.applied.filed as usize + r.applied.deleted as usize > 0 => console.event(&format!(
                "Filing sync, {account}: {} of this computer's sent, {} filed as other computers did, {} removed, {} not here yet.",
                thousands(r.sent as u64),
                thousands(r.applied.filed),
                thousands(r.applied.deleted),
                thousands(r.applied.missing)
            )),
            Ok(_) => {}
            Err(e) => console.event(&format!("Filing sync, {account}: didn't run ({e}); trying again later.")),
        }
        if let Ok(r) = result {
            if !r.broken.is_empty() {
                console.event(&format!("Filing sync, {account}: the copy from {} can't be read (damaged or cut short).", r.broken.join(", ")));
            }
            if r.asked {
                console.event(&format!("Filing sync, {account}: asked your other computers for a full copy (SOS)."));
            }
            if r.answered > 0 {
                console.event(&format!("Filing sync, {account}: stored a full copy for {} computer(s) that asked (SOS).", r.answered));
            }
            if r.parts > 1 {
                console.event(&format!("Filing sync, {account}: stored in {} parts.", r.parts));
            }
        }
    }
}

/// "4GB", "700MB", "4.7 GB", "1000000" (bytes).
fn parse_size(text: &str) -> anyhow::Result<u64> {
    let t = text.trim().to_uppercase().replace(' ', "");
    let (num, mult) = match t.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => (&t[..i], match &t[i..] {
            "B" => 1.0,
            "KB" | "K" => 1e3,
            "MB" | "M" => 1e6,
            "GB" | "G" => 1e9,
            "TB" | "T" => 1e12,
            other => anyhow::bail!("unknown size unit {other} (use MB or GB)"),
        }),
        None => (t.as_str(), 1.0),
    };
    let n: f64 = num.parse().with_context(|| format!("{text} isn't a size"))?;
    Ok((n * mult) as u64)
}

fn print_download(r: &eudora_core::graph_sync::SyncReport, console: &Console) {
    // what couldn't be downloaded goes into the log too: the window's download has no console, and
    // 32,750 archive messages failed unseen for weeks (2026-10-25). Each pass says plainly whether
    // everything on the servers is here: an item not downloaded, or a folder that couldn't be
    // listed, counts as missing; folders passed over were complete when last listed
    if r.failed > 0 {
        console.log_line(&format!("{}: {} new; {} items on the server aren't here yet (tried again next pass):", r.account, thousands(r.downloaded), thousands(r.failed)), false);
    } else if !r.canceled {
        console.log_line(&format!("{}: {} new; every item on the server is here.", r.account, thousands(r.downloaded)), false);
    }
    for f in &r.failures {
        console.log_line(&format!("  {f}"), false);
    }
    {
            let c = &r.counts;
            println!(
                "{} {}: {} folders, {} items listed, {} already here, {} downloaded in {:.0} min.",
                if r.canceled { "Stopped" } else { "Downloaded" },
                r.account,
                thousands(r.folders as u64),
                thousands(r.listed),
                thousands(r.already),
                thousands(r.downloaded),
                r.elapsed_secs / 60.0
            );
            println!(
                "  {} new messages; {} copies of messages already in the library (same Message-ID); {} identical, now also in this folder; {} identical in the same folder.",
                thousands(c.imported),
                thousands(c.merged),
                thousands(c.new_locations),
                thousands(c.duplicates)
            );
            println!(
                "  {} fixed while reading, {} unreadable (kept as text), {} archive items not converted (kept with the export attached), {} not mail (left on the server).",
                thousands(c.fixed),
                thousands(c.unreadable),
                thousands(r.unconverted),
                thousands(r.not_mail)
            );
            if c.copies_kept > 0 {
                println!("  {} original copies kept for messages already here.", thousands(c.copies_kept));
            }
            if r.reconverted > 0 {
                println!("  {} archive items an earlier run couldn't convert were converted now.", thousands(r.reconverted));
            }
            if r.pop_deleted > 0 {
                println!("  {} messages deleted from the POP3 server (kept here: the account deletes them after the days set).", thousands(r.pop_deleted));
            }
            if r.left_big > 0 {
                println!("  {} messages over the account's size limit left on the server.", thousands(r.left_big));
            }
            if r.failed > 0 {
                println!("  {} couldn't be downloaded this time (run the same command again to retry them).", thousands(r.failed));
            }
            // (every note: failures, folders that couldn't be read, messages left on the server)
            for f in r.failures.iter().take(20) {
                println!("    {}", cut(f, 160));
            }
    }
}

/// Compares the library with the corpus manifest; times searches and folder pages.
fn check_corpus(cli: &Cli, corpus: &std::path::Path, console: &Console) -> Result<()> {
    let manifest: eudora_testkit::Manifest = serde_json::from_slice(&std::fs::read(corpus.join("manifest.json"))?)?;
    let library = Library::open(&library_path(cli)?, Access::Read, console)?;
    let stats = library.stats()?;
    let mut failures = 0;
    let mut check = |what: &str, got: u64, want: u64| {
        let ok = got == want;
        if !ok {
            failures += 1;
        }
        println!("{} {what}: {} (expected {})", if ok { "OK  " } else { "FAIL" }, thousands(got), thousands(want));
    };
    check("messages stored", stats.messages, manifest.unique);
    check("locations (messages in folders)", stats.locations, manifest.written - manifest.duplicates);
    check("search index entries", stats.indexed_docs, manifest.unique);
    let base = "Imports/mail";
    for (folder, want) in &manifest.folders {
        let path = format!("{base}/{folder}");
        let got = library.find_folder(&path)?.map(|id| library.folders().map(|fs| fs.iter().find(|f| f.id == id).map_or(0, |f| f.messages))).transpose()?.unwrap_or(0);
        check(&format!("folder {path}"), got, *want);
    }
    println!();
    for needle in eudora_testkit::NEEDLES {
        let want = manifest.needles.get(needle.name).copied().unwrap_or(0);
        let (result, _) = library.search(needle.query, 10, SortOrder::Newest)?;
        check(&format!("search {} ({:.1} ms)", needle.query, result.took.as_secs_f64() * 1000.0), result.total as u64, want);
    }
    println!();
    // Speed: common searches and folder pages (the targets: search < 200 ms, a page < 100 ms).
    for query in ["meeting", "invoice AND attached", "from:smith", "\"thanks regards\"", "subject:report NOT budget", "date:[2001-01-01T00:00:00Z TO 2002-01-01T00:00:00Z]"] {
        let (result, _) = library.search(query, 50, SortOrder::Newest)?;
        println!("TIME search {query:<55} {:>10} matches {:>8.1} ms", thousands(result.total as u64), result.took.as_secs_f64() * 1000.0);
    }
    if let Some(inbox) = library.find_folder(&format!("{base}/Inbox"))? {
        let started = Instant::now();
        let first = library.list_folder(inbox, None, 50)?;
        let t1 = started.elapsed();
        let mut cursor = first.last().map(|r| PageCursor { date: r.date, id: r.id });
        let started = Instant::now();
        let mut pages = 0;
        while pages < 200 {
            let page = library.list_folder(inbox, cursor, 50)?;
            match page.last() {
                Some(r) => cursor = Some(PageCursor { date: r.date, id: r.id }),
                None => break,
            }
            pages += 1;
        }
        let per_page = started.elapsed().as_secs_f64() * 1000.0 / pages.max(1) as f64;
        println!("TIME Inbox first page of 50: {:.1} ms; next {pages} pages (scrolling): {per_page:.2} ms each", t1.as_secs_f64() * 1000.0);
    }
    if failures > 0 {
        bail!("{failures} checks failed");
    }
    println!("\nAll checks passed.");
    Ok(())
}

fn graph(command: &GraphCommand) -> Result<()> {
    match command {
        GraphCommand::SharedCheck { addresses, file } => {
            let client = GraphClient::resume(None, eudora_core::graph::ALL_SCOPES)?;
            let mut list: Vec<String> = addresses.clone();
            if let Some(file) = file {
                list.extend(std::fs::read_to_string(file)?.lines().map(str::trim).filter(|l| l.contains('@')).map(str::to_string));
            }
            let mut open = 0;
            for (n, address) in list.iter().enumerate() {
                let mut c = client.worker();
                c.set_mailbox(Some(address.clone()));
                match c.mailbox_access() {
                    Ok(inbox) => {
                        open += 1;
                        println!("{:>4} of {}  OPEN     {address:<40} Inbox: {}", n + 1, list.len(), thousands(inbox));
                    }
                    Err(e) => println!("{:>4} of {}  no       {address:<40} {}", n + 1, list.len(), cut(&e.to_string(), 70)),
                }
            }
            println!("You can open {open} of {} mailboxes.", list.len());
        }
        GraphCommand::AddShared { .. } | GraphCommand::Changes { .. } | GraphCommand::ArchiveRefile { .. } | GraphCommand::ArchiveMissing { .. } | GraphCommand::Failed { .. } => unreachable!("handled in main (needs the library)"),
        GraphCommand::Answered { account, all } => {
            let mut client = GraphClient::resume_best(account.as_deref(), eudora_core::graph::ALL_SCOPES)?;
            println!("Signed in as {}", client.account);
            let started = std::time::Instant::now();
            match client.last_verbs(!*all, 2000) {
                Ok(found) => {
                    let count = |v: i64| found.iter().filter(|(_, x)| *x == v).count() as u64;
                    println!(
                        "Of the newest 2,000 {}: replied {}, replied to all {}, forwarded {} ({:.1} s)",
                        if *all { "messages" } else { "in the Inbox" },
                        thousands(count(102)),
                        thousands(count(103)),
                        thousands(count(104)),
                        started.elapsed().as_secs_f32()
                    );
                }
                Err(e) => println!("Microsoft refused: {e}"),
            }
        }
        GraphCommand::Contacts { account } => {
            let mut client = GraphClient::resume_best(account.as_deref(), eudora_core::graph::ALL_SCOPES)?;
            println!("Signed in as {}", client.account);
            match client.address_books() {
                Ok(books) => {
                    for (_, name, contacts) in &books {
                        println!("{name}: {} contacts", thousands(contacts.len() as u64));
                    }
                }
                Err(e) => println!("Microsoft refused: {e}"),
            }
        }
        GraphCommand::Calendar { account } => {
            let mut client = GraphClient::resume_best(account.as_deref(), eudora_core::graph::ALL_SCOPES)?;
            println!("Signed in as {}", client.account);
            let now = chrono::Utc::now().timestamp();
            match client.calendars() {
                Ok(list) => {
                    for cal in &list {
                        let name = cal["name"].as_str().unwrap_or("?");
                        match client.calendar_view(cal["id"].as_str().unwrap_or(""), now, now + 30 * 86_400) {
                            Ok(events) => {
                                println!("{name}: {} events in the next 30 days", thousands(events.len() as u64));
                                for v in events.iter().take(5) {
                                    let e = eudora_core::calendar::from_graph(v);
                                    let at = chrono::DateTime::from_timestamp(e.start, 0).map(|t| t.with_timezone(&chrono::Local).format("%a %b %-d %-I:%M %p").to_string()).unwrap_or_default();
                                    println!("  {at}  {}{}", e.subject, if e.all_day { " (all day)" } else { "" });
                                }
                            }
                            Err(e) => println!("{name}: Microsoft refused: {e}"),
                        }
                    }
                }
                Err(e) => println!("Microsoft refused: {e}"),
            }
        }
        GraphCommand::SharedFind => {
            let mut client = GraphClient::resume(None, eudora_core::graph::ALL_SCOPES)?;
            let console = Console::new(None)?;
            let open = eudora_core::graph_sync::find_mailboxes(&mut client, &console)?;
            console.done();
            for m in &open {
                println!("{:<40} {:<34} Inbox: {}", m.address, cut(&m.name, 34), thousands(m.inbox));
            }
            println!("You can open {} mailboxes. Add the ones you want: eudora-cli graph add-shared ADDRESS…", open.len());
        }
        GraphCommand::Login { archive, account } => {
            let show = |url: &str| {
                println!("Opening Microsoft's sign-in page in your browser…\n(If it doesn't open, copy this address into it:)\n{url}\n");
                let _ = eudora_core::graph::open_browser(url);
            };
            // everything (a personal account: what it allows), or read-only
            let client = if *archive { GraphClient::sign_in_any(&show, account.as_deref())? } else { GraphClient::sign_in_as(&show, eudora_core::graph::SCOPES, account.as_deref())? };
            println!("Signed in as {} <{}>. The sign-in is kept in this computer's credential store.", client.display_name, client.account);
        }
        GraphCommand::ArchiveSample { folder, count, out } => {
            let mut client = GraphClient::resume(None, eudora_core::graph::ARCHIVE_SCOPES)?;
            let (_primary, archive) = client.mailbox_ids()?;
            let archive = archive.context("this account has no Online Archive")?;
            let folders = client.mailbox_folders(&archive, &|_| {})?;
            // the folder by its path ("Old Sent Items/1999"): walk the depth-first list
            let mut path: Vec<String> = Vec::new();
            let mut found = None;
            for f in &folders {
                path.truncate(f.depth);
                path.push(f.display_name.clone());
                if path.join("/").eq_ignore_ascii_case(folder.trim_matches('/')) {
                    found = Some(f.clone());
                    break;
                }
            }
            let f = found.with_context(|| format!("there's no folder \"{folder}\" in the archive"))?;
            let owner = GraphClient::folder_owner(&archive, &f);
            let items = client.folder_items(&owner, &f.id, *count)?;
            println!("{}: {} items (showing {})", folder, thousands(f.total_item_count), items.len());
            let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
            let started = Instant::now();
            let exported = client.export_items(&owner, &ids)?;
            println!("Exported {} in {:.0} ms", exported.len(), started.elapsed().as_secs_f64() * 1000.0);
            std::fs::create_dir_all(out)?;
            for (n, (item, e)) in items.iter().zip(&exported).enumerate() {
                match (&e.data, &e.error) {
                    (Some(data), _) => {
                        let path = out.join(format!("{:02}.fts", n + 1));
                        std::fs::write(&path, data)?;
                        let head: String = data.iter().take(24).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
                        println!("  {} {:<14} {:>9} bytes  {}  {}", path.display(), item.kind.as_deref().unwrap_or(""), data.len(), item.created_date_time.as_deref().unwrap_or(""), head);
                    }
                    (None, err) => println!("  item {}: {}", n + 1, err.as_deref().unwrap_or("no data")),
                }
            }
        }
        GraphCommand::Convert { files, out } => {
            let (mut done, mut failed) = (0usize, 0usize);
            for (n, file) in files.iter().enumerate() {
                let export = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
                let item = match eudora_core::fts::parse(&export) {
                    Ok(item) => item,
                    Err(e) => {
                        failed += 1;
                        println!("File {} of {}: {}: {e}", n + 1, files.len(), file.display());
                        continue;
                    }
                };
                let mime = eudora_core::fts::to_mime(&item);
                let target = match out {
                    Some(dir) => {
                        std::fs::create_dir_all(dir)?;
                        dir.join(file.with_extension("eml").file_name().unwrap_or_default())
                    }
                    None => file.with_extension("eml"),
                };
                std::fs::write(&target, &mime)?;
                let m = eudora_core::parse::parse(&mime, Default::default(), eudora_core::library::now());
                done += 1;
                println!("File {} of {}: {} -> {} ({} bytes; {} recipients, {} attachments)", n + 1, files.len(), file.display(),
                    target.display(), thousands(mime.len() as u64), item.recipients.len(), item.attachments.len());
                println!("    {}  {}  {}  <{}>", date(m.date), cut(&m.from_display(), 30), cut(&m.subject, 50), m.message_id);
            }
            println!("Converted {done}, failed {failed}.");
        }
        GraphCommand::Archive => {
            let mut client = GraphClient::resume(None, eudora_core::graph::ARCHIVE_SCOPES)?;
            println!("{} <{}>", client.display_name, client.account);
            let (_primary, archive) = client.mailbox_ids()?;
            let Some(archive) = archive else {
                println!("This account has no Online Archive.");
                return Ok(());
            };
            let started = Instant::now();
            let folders = client.mailbox_folders(&archive, &|n| eprint!("\rReading the archive's folders: {n}…   "))?;
            eprintln!();
            let mb = |b: u64| format!("{:.1} MB", b as f64 / 1e6);
            let (mut items, mut bytes, mut auxiliary) = (0u64, 0u64, 0usize);
            for f in &folders {
                items += f.total_item_count;
                bytes += f.size.unwrap_or(0);
                if f.parent_mailbox_url.as_deref().is_some_and(|u| !u.contains(&archive)) {
                    auxiliary += 1;
                }
                println!("{:>10}  {:>12}  {}{}{}", thousands(f.total_item_count), f.size.map(mb).unwrap_or_default(),
                    "  ".repeat(f.depth), f.display_name, if f.is_hidden { "  (hidden)" } else { "" });
            }
            println!("{:>10}  {:>12}  items in {} folders of the Online Archive{} (read in {:.1} s)", thousands(items),
                if bytes > 0 { format!("{:.1} GB", bytes as f64 / 1e9) } else { "size n/a".into() }, folders.len(),
                if auxiliary > 0 { format!(", {auxiliary} of them in auxiliary archive mailboxes") } else { String::new() },
                started.elapsed().as_secs_f64());
        }
        GraphCommand::Folders => {
            let mut client = GraphClient::resume(None, eudora_core::graph::SCOPES)?;
            println!("{} <{}>", client.display_name, client.account);
            let started = Instant::now();
            let folders = client.folders()?;
            let (mut total, mut unread) = (0u64, 0u64);
            for f in &folders {
                total += f.total_item_count;
                unread += f.unread_item_count;
                println!("{:>10}  {:>8}  {}{}{}", thousands(f.total_item_count), if f.unread_item_count > 0 { thousands(f.unread_item_count) } else { String::new() },
                    "  ".repeat(f.depth), f.display_name, if f.is_hidden { "  (hidden)" } else { "" });
            }
            println!("{:>10}  {:>8}  messages / unread in {} folders (read in {:.1} s)", thousands(total), thousands(unread), folders.len(), started.elapsed().as_secs_f64());
        }
        GraphCommand::Newest { folder, count } => {
            let mut client = GraphClient::resume(None, eudora_core::graph::SCOPES)?;
            for m in client.newest(folder, *count)? {
                println!("{}  {:<36}  {}", m.received_date_time.as_deref().unwrap_or("").get(..16).unwrap_or(""),
                    cut(&m.from_display(), 36), cut(m.subject.as_deref().unwrap_or(""), 70));
            }
        }
        GraphCommand::Fetch { id } => {
            let mut client = GraphClient::resume(None, eudora_core::graph::SCOPES)?;
            let id = match id {
                Some(id) => id.clone(),
                None => client.newest("inbox", 1)?.into_iter().next().context("the Inbox is empty")?.id,
            };
            let started = Instant::now();
            let raw = client.raw_message(&id)?;
            let took = started.elapsed();
            let m = eudora_core::parse::parse(&raw, Default::default(), eudora_core::library::now());
            println!("Downloaded {} bytes in {:.0} ms and read it:", thousands(raw.len() as u64), took.as_secs_f64() * 1000.0);
            println!("  From:    {}", m.from_display());
            println!("  Date:    {}", date(m.date));
            println!("  Subject: {}", m.subject);
            println!("  Message-ID: {}", m.message_id);
            println!("  Attachments: {}", m.attachments.iter().map(|a| format!("{} ({} bytes)", a.name, a.size)).collect::<Vec<_>>().join(", "));
            println!("  Text: {}", cut(&m.snippet, 160));
        }
        GraphCommand::Download { .. } => unreachable!("handled in main (needs the library)"),
        GraphCommand::Logout => {
            let client_account = GraphClient::resume(None, eudora_core::graph::SCOPES).map(|c| c.account).unwrap_or_default();
            if !client_account.is_empty() {
                GraphClient::sign_out(&client_account)?;
                println!("Signed out {client_account} on this computer.");
            } else {
                println!("No Microsoft account was signed in.");
            }
        }
    }
    Ok(())
}

/// Accounts named by number or address, as ids.
fn account_ids(library: &Library, names: &[String]) -> eudora_core::Result<Vec<i64>> {
    let all = library.accounts()?;
    names
        .iter()
        .map(|n| {
            let n = n.trim();
            n.parse::<i64>()
                .ok()
                .filter(|id| all.iter().any(|a| a.id == *id))
                .or_else(|| all.iter().find(|a| a.address.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(n))).map(|a| a.id))
                .ok_or_else(|| eudora_core::Error::Other(format!("there's no account “{n}” (eudora-cli accounts lists them)")))
        })
        .collect()
}
