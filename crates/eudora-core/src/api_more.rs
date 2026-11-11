//! The rest of what Eudora's window does, as API methods, each behind its permission, scoped,
//! rate-limited and logged by `api.rs` like the first ones. The command line reaches every one
//! of them as the owner (`eudora call METHOD JSON`).
//!
//! Left out on purpose (they'd undo the security itself): granting permissions or answering a
//! program's requests and sends (the user's alone, in the window), reading secrets (the sync
//! recovery phrase, passwords, sign-in tokens; a backup through the API never holds
//! passwords). And what is the window itself: the Open dialog, opening windows and links.

use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::api::{Client, Host, Method, Permission, Refusal, Scope};
use crate::error::Error;
use crate::library::{Library, PageCursor};

type R<T> = std::result::Result<T, Refusal>;

const fn m(name: &'static str, permission: Permission, write: bool, params: &'static str, summary: &'static str) -> Method {
    Method { name, permission: Some(permission), write, params, summary }
}

use Permission as P;

/// The rest of version 1.
pub const MORE: &[Method] = &[
    // what Eudora is doing
    m("v1.library.info", P::StatusRead, false, "{}", "The library: where it is, its oldest and newest mail"),
    m("v1.library.jobs", P::StatusRead, false, "{}", "What the download is doing in the background, job by job: what each last did, when, how many this session, how many left"),
    m("v1.library.activity", P::StatusRead, false, "{}", "Download state, changes waiting for the servers, the Out mailbox, offline, paused"),
    m("v1.library.pending", P::StatusRead, false, "{}", "Changes waiting for the servers, by kind"),
    m("v1.library.server_changes", P::StatusRead, false, "{}", "Whether Eudora may change mail on Microsoft 365, and the moves waiting"),
    m("v1.library.stats", P::StatusRead, false, "{}", "Statistics: received and sent, busiest days, kinds of mail, storage"),
    m("v1.library.pause_state", P::StatusRead, false, "{}", "Whether downloading is paused (none, old, all)"),
    m("v1.library.maintenance", P::StatusRead, false, "{}", "Maintenance: its schedule (off, daily, weekly; the hour; only when idle), its last run, whether one is asked for"),
    m("v1.library.speed", P::StatusRead, false, "{}", "How fast things are: searches, list pages, opening messages, storing new mail, maintenance (count, average, median, slowest 5%)"),
    m("v1.sync.state", P::StatusRead, false, "{}", "Sync between computers: set up here, its home account and carriers"),
    m("v1.firewall.status", P::StatusRead, false, "{}", "Whether phones may reach the web interface through the firewall"),
    m("v1.plugins.list", P::StatusRead, false, "{}", "Installed plugins"),
    m("v1.updates.status", P::StatusRead, false, "{}", "Updates: this version, the choices (check on start, every N days), how this copy updates, an update waiting"),
    m("v1.updates.check", P::LibraryManage, true, "{force?: bool}", "Looks for a newer Eudora now (force, the default) or only if due, and downloads it when this copy can install it"),
    m("v1.updates.install", P::LibraryManage, true, "{when: now | close | skip}", "Answers a downloaded update: install now (Eudora closes and starts again), when Eudora closes, or skip this version"),
    m("v1.updates.settings_set", P::SettingsWrite, true, "{on_start: bool, every_days: 0-365}", "When Eudora looks for updates by itself"),
    // running the library
    m("v1.library.check_mail", P::LibraryManage, true, "{}", "Check Mail: every mailbox looks for new mail now"),
    m("v1.library.maintain", P::LibraryManage, true, "{}", "Runs maintenance now (the download takes it within seconds): counts checked, deleted mail's space given back, the search index merged, the database's statistics, Help ▸ Statistics"),
    m("v1.library.maintenance_set", P::LibraryManage, true, "{schedule: {when: off | daily | weekly, hour: 0-23, weekday: 0-6, idle_only}}", "Sets when maintenance runs by itself"),
    m("v1.library.look", P::LibraryManage, true, "{folders: [..]}", "Brings these folders up to date now (their newest messages), as opening them in the window does"),
    m("v1.library.pause", P::LibraryManage, true, "{mode: none | old | all}", "Pauses downloading old mail, all of it, or resumes"),
    m("v1.library.job_log", P::StatusRead, false, "{job, before?, runs?}", "What a background job did to each message: its latest run (or the runs before `before`), with each message's sender, subject and date; `more` pages back"),
    m("v1.library.job_pause", P::LibraryManage, true, "{key, paused}", "Pauses or resumes one background job (its key from v1.library.jobs: views, headers, travel, alerts, payments, archive, server, rules, maintenance…)"),
    m("v1.library.offline", P::LibraryManage, true, "{on?: bool}", "Work Offline: on, off, or (nothing) says which"),
    m("v1.library.background", P::LibraryManage, true, "{options?: {on, archive, shared}}", "The window's background download: its state, or new options"),
    m("v1.library.backup_info", P::LibraryManage, false, "{}", "What a backup would hold (sizes by kind)"),
    m("v1.library.backup", P::LibraryManage, true, "{dest, password?, indexes?: bool, part_gb?: number}", "Backs up the whole library to a file (never with passwords through the API)"),
    m("v1.library.restore_describe", P::LibraryManage, false, "{path}", "What a backup holds"),
    m("v1.library.restore_destination", P::LibraryManage, false, "{dest}", "What a restore's folder holds: empty, a Eudora library (replace: true replaces it), or other files"),
    m("v1.library.restore", P::LibraryManage, true, "{path, dest?, password?, replace?: bool}", "Restores a backup into an empty folder (else the usual place), or in place of the Eudora library there (replace); it becomes the library Eudora opens from then on; never inside Dropbox, OneDrive or another sync folder"),
    m("v1.library.import", P::LibraryManage, true, "{paths: [..], into?, account?, merge?: bool}", "Imports mailbox files, messages or a classic Eudora folder"),
    m("v1.library.optimize", P::LibraryManage, true, "{vacuum?: bool}", "Optimize Library: gives back the space of deleted mail"),
    m("v1.library.old_attachments", P::LibraryManage, true, "{folders?: [..]}", "Finds classic Eudora's attachment files still missing"),
    m("v1.sync.setup", P::LibraryManage, true, "{}", "Starts sync on this computer (its recovery phrase is shown only in the window)"),
    m("v1.sync.join", P::LibraryManage, true, "{phrase}", "Joins sync with the recovery phrase from another computer"),
    m("v1.sync.carriers", P::LibraryManage, true, "{home, carriers: [..]}", "The home account and the others that carry the settings"),
    m("v1.sync.now", P::LibraryManage, true, "{repair?: bool}", "Syncs now (repair: asks the other computers for a full copy)"),
    m("v1.firewall.set", P::LibraryManage, true, "{port, allow: bool}", "Lets phones on this network reach the web interface (Windows asks the user)"),
    m("v1.plugins.install", P::LibraryManage, true, "{path}", "Installs a plugin; the user is asked what it may do"),
    m("v1.plugins.enable", P::LibraryManage, true, "{id, on: bool}", "Switches a plugin on or off"),
    m("v1.plugins.remove", P::LibraryManage, true, "{id}", "Removes a plugin and its permissions"),
    m("v1.plugins.run", P::LibraryManage, true, "{id, command, context?}", "Runs a plugin's command (it sees only what it was granted)"),
    m("v1.rules.import_classic", P::LibraryManage, false, "{path, account}", "Reads classic Eudora's Filters.pce: its rules (not saved)"),
    // accounts
    m("v1.accounts.identities", P::AccountsRead, false, "{}", "Every address mail can be sent from"),
    m("v1.accounts.sign_ins", P::AccountsRead, false, "{}", "Which sign-ins and passwords this computer keeps (names only, never the secrets)"),
    m("v1.accounts.missing_sign_ins", P::AccountsRead, false, "{}", "Accounts this computer has no sign-in or password for yet (after a restore): [{address, kind: microsoft | google | password, kind_name}]"),
    m("v1.accounts.check_every", P::AccountsRead, false, "{}", "How often each account is checked for new mail: {accounts: {address: minutes} (those not at the default), default, choices}"),
    m("v1.accounts.check_every_set", P::AccountsManage, true, "{address, minutes}", "How often an account is checked for new mail (1, 5, 10, 15, 30 or 60 minutes)"),
    m("v1.accounts.download_period", P::AccountsRead, false, "{}", "How much mail is downloaded from the servers: {chosen, days, mb} (a period in days, or a space in megabytes; both null = everything; chosen false = never asked, everything)"),
    m("v1.accounts.download_period_set", P::AccountsManage, true, "{days, mb}", "How much mail to download from the servers and keep on this computer: the last N days (any number), or up to N megabytes of space (newest first), or both null for everything. Older mail then leaves this computer once its server confirms it still has it; mail only here (POP3, imported files, classic Eudora) always stays"),
    m("v1.accounts.keep_preview", P::AccountsRead, false, "{days}", "What keeping only the last N days on this computer would mean, changing nothing: {before, older, older_bytes, removable, removable_bytes, only_here, flagged_or_waiting, messages, library_bytes}"),
    m("v1.accounts.set_name", P::AccountsManage, true, "{id, name}", "An account's own name (empty: its address)"),
    m("v1.accounts.readdress", P::AccountsManage, true, "{id, address}", "Changes the address of an account made from files"),
    m("v1.accounts.imap_list", P::AccountsManage, false, "{}", "IMAP / POP3 accounts' server settings (no passwords)"),
    m("v1.accounts.mail_host", P::AccountsRead, false, "{address}", "Who hosts an address's mail: microsoft (sign in with Microsoft), google, or null"),
    m("v1.accounts.imap_test", P::AccountsManage, true, "{config, password, smtp_password?}", "Signs in to a mail server with these settings (nothing saved)"),
    m("v1.accounts.imap_save", P::AccountsManage, true, "{config, password?, smtp_password?}", "Adds or changes an IMAP / POP3 account"),
    m("v1.accounts.find_shared", P::AccountsManage, false, "{}", "Microsoft 365 mailboxes the user can open"),
    m("v1.accounts.add_shared", P::AccountsManage, true, "{addresses: [..]}", "Adds shared mailboxes as accounts"),
    m("v1.accounts.microsoft_sign_in", P::AccountsManage, true, "{address?}", "Opens Microsoft's sign-in in the browser (the user signs in there); `address`: the account meant"),
    m("v1.accounts.google_sign_in", P::AccountsManage, true, "{address?}", "Opens Google's sign-in in the browser (the user signs in there): Gmail, contacts, calendars"),
    m("v1.accounts.google_available", P::AccountsRead, false, "{}", "Whether this copy of Eudora has Google's sign-in built in"),
    m("v1.accounts.forget_sign_ins", P::AccountsManage, true, "{}", "Forget Sign-ins: removes this computer's stored sign-ins and passwords"),
    // reading mail
    m("v1.mail.page", P::HeadersRead, false, "{folders?: [..], view?, accounts?: [..], hide_sent?: bool, after?: {date, id}, limit?}", "A list's messages a page at a time: folders (a label), All Mail, or a Synthetic View"),
    m("v1.mail.count", P::HeadersRead, false, "{folders?: [..], view?, accounts?: [..], hide_sent?: bool}", "How many messages a list holds"),
    m("v1.mail.sorted", P::HeadersRead, false, "{folders?: [..], view?, accounts?: [..], hide_sent?: bool, sort: date|added|size|attachments|status|priority|subject|from|to|cc|preview|list|msgid, ascending?: bool, offset?, limit?}", "A list sorted by a column, a page at a time"),
    m("v1.mail.ids", P::HeadersRead, false, "{folders?: [..], view?, accounts?: [..], hide_sent?: bool}", "Every message of a list with a folder it's in"),
    m("v1.mail.header_values", P::HeadersRead, false, "{ids, names: [..]}", "These headers' values for these messages"),
    m("v1.mail.copies", P::HeadersRead, false, "{id}", "Where a message's original copies came from"),
    m("v1.mail.links", P::HeadersRead, false, "{id}", "The payment or receipt linked to a message"),
    m("v1.views.list", P::HeadersRead, false, "{accounts?: [..]}", "Synthetic Views (Receipts, Invoices, Red Alert…) with their counts"),
    m("v1.views.groups", P::HeadersRead, false, "{view, accounts?: [..]}", "A view's groups by sender"),
    m("v1.views.why", P::BodiesRead, false, "{id}", "Why a message is (or isn't) in each view: every view's points and reasons (2 points puts it in)"),
    m("v1.subscriptions.list", P::HeadersRead, false, "{}", "Mailing lists, notifications and marketing the user gets (proven senders only, no spam or fraud), with how often"),
    m("v1.subscriptions.unsubscribe", P::Change, true, "{key, newest}", "Unsubscribes: Eudora does a one-click unsubscribe itself, else answers with the page to open or the address to write"),
    m("v1.mail.attachment_save", P::AttachmentsRead, true, "{id, part, dir}", "Saves one attachment into a folder (marked as downloaded from the Internet); returns where"),
    m("v1.mail.attachments_save_all", P::AttachmentsRead, true, "{id, dir}", "Saves every attachment of a message into a folder; returns where, and any classic Eudora file never found"),
    m("v1.mail.save_as", P::BodiesRead, true, "{ids, dir, format?: msg | eml}", "Saves messages into a folder as files named by their subjects: msg = Outlook's .msg (the header block and attachments exact; Exchange's own properties for Online Archive items), eml = the original bytes exactly; returns where each went"),
    m("v1.contacts.list", P::ContactsRead, false, "{query?, account?}", "The address book: every contact of every account (or one account), those matching the words, by name"),
    m("v1.contacts.origin", P::ContactsRead, false, "{id}", "Where a contact came from (account, service, address book) and whether a change of yours is still on its way to the server"),
    m("v1.contacts.update", P::ContactsWrite, true, "{id, card}", "Changes a contact (here at once, on its server through the queue)"),
    m("v1.contacts.create", P::ContactsWrite, true, "{account, book?, card}", "Adds a contact to an account's address book (book: its server id; none = the default one)"),
    m("v1.contacts.delete", P::ContactsWrite, true, "{id}", "Deletes a contact (here at once, on its server through the queue; kept in the history)"),
    m("v1.contacts.share", P::ContactsRead, true, "{id}", "A draft with the contact attached as a vCard, to address and send; returns the draft's id"),
    m("v1.contacts.counts", P::ContactsRead, false, "{}", "How many contacts each account's address books hold"),
    m("v1.contacts.get", P::ContactsRead, false, "{id}", "One contact, with the account and address book it's in"),
    m("v1.contacts.export", P::ContactsRead, true, "{path, account?}", "Writes the address book to a file: vCard (.vcf) or CSV (.csv), by the file's name"),
    m("v1.calendar.calendars", P::CalendarRead, false, "{}", "Every calendar of every account: name, color, whether it's shown, how many events"),
    m("v1.calendar.events", P::CalendarRead, false, "{from, to}", "The events between two times (Unix seconds), each occurrence of a series on its own"),
    m("v1.calendar.get", P::CalendarRead, false, "{id}", "One event whole: people, answers, reminder, meeting link"),
    m("v1.calendar.search", P::CalendarRead, false, "{words, limit?}", "Events whose subject, place or people match the words, newest first"),
    m("v1.calendar.create", P::CalendarWrite, true, "{calendar, subject, start, end, all_day?, location?, place?, conference?: teams | meet | zoom, google?, attendees?, notes?, reminder?, show_as?, private?}", "Makes an appointment (Microsoft sends its people the invitations; a Meet or Zoom meeting's details go in it)"),
    m("v1.calendar.appointment", P::CalendarRead, false, "{id}", "An appointment for editing: the event, its text when here, its map link, whether it's an online meeting"),
    m("v1.calendar.edit", P::CalendarWrite, true, "{id, event: {as create}, body_changed?: bool}", "Changes an appointment you organized (Microsoft sends its people the update)"),
    m("v1.calendar.ics", P::CalendarRead, false, "{id}", "An event as an .ics file's text"),
    m("v1.calendar.conference_options", P::CalendarRead, false, "{calendar}", "Which online meetings can go in an event of this calendar: Teams, Google Meet (accounts), Zoom"),
    m("v1.calendar.places", P::CalendarRead, false, "{text}", "Places matching the text (Google Maps with your key, else OpenStreetMap)"),
    m("v1.calendar.shown", P::CalendarWrite, true, "{id, shown}", "Shows or hides a calendar in the views"),
    m("v1.calendar.color", P::CalendarWrite, true, "{id, color: \"#rrggbb\" | null}", "A calendar's color in Eudora (null: the calendar's own color)"),
    m("v1.calendar.reservations", P::CalendarRead, false, "{}", "Reservations ahead found in Travel mail (flights, hotel stays): added, already there, or waiting for an answer"),
    m("v1.calendar.invite_spam", P::CalendarWrite, true, "{event, series?: bool}", "Reports an invitation as spam: no answer is sent; the meeting leaves the calendar, its email goes to Spam and the sender is blocked"),
    m("v1.calendar.invites", P::CalendarRead, false, "{}", "Invitations ahead waiting for an answer, each with what else is on the calendar then"),
    m("v1.calendar.invitation", P::CalendarRead, false, "{id}", "A message's invitation (its invite file) and its event on the calendar"),
    m("v1.calendar.invitation_add", P::CalendarWrite, true, "{id, calendar?}", "Adds an invitation email's meeting to a calendar (the account's default if none given), without its people: nobody is sent anything"),
    m("v1.calendar.reservations_answer", P::CalendarWrite, true, "{add: [keys], decline: [keys]}", "Puts offered reservations on the calendar, or declines them (not asked again)"),
    m("v1.calendar.reservation_off", P::CalendarWrite, true, "{key}", "Takes a reservation's event off the calendar (it stays in Trips)"),
    m("v1.calendar.reservation_hide", P::CalendarWrite, true, "{key}", "Removes a reservation from Trips"),
    m("v1.calendar.trips_from_message", P::CalendarWrite, true, "{id}", "Reads one email for trips (Add to Trips): how many reservations it holds"),
    m("v1.todo.list", P::TodoRead, false, "{}", "Every To Do task: open ones by due date, then the last month's done ones"),
    m("v1.todo.lists", P::TodoRead, false, "{}", "The To Do lists: whose (owner), shown or hidden, open tasks"),
    m("v1.todo.list_shown", P::TodoWrite, true, "{id, shown}", "Shows or hides a To Do list in Eudora (Microsoft To Do keeps it either way)"),
    m("v1.todo.list_color", P::TodoWrite, true, "{id, color: \"#rrggbb\" | null}", "A To Do list's color in Eudora (null: its first color)"),
    m("v1.todo.add", P::TodoWrite, true, "{title, notes?, due?, message?}", "A new task (due: Unix seconds; message: the message it's about); it goes to Microsoft To Do"),
    m("v1.todo.update", P::TodoWrite, true, "{id, title?, notes?, due?, done?, important?}", "Changes a task (due: null takes the date off)"),
    m("v1.todo.delete", P::TodoWrite, true, "{id}", "Deletes a task (a flagged message loses its flag)"),
    m("v1.todo.flag", P::TodoWrite, true, "{ids, on}", "Flags messages as To Do items (Outlook's flag on Microsoft 365 mail), or takes the flag off"),
    m("v1.todo.twos", P::TodoWrite, true, "{id}", "Sends a task to Twos' today list (set up in Settings)"),
    m("v1.calendar.respond", P::CalendarWrite, true, "{id, answer: accept | tentative | decline, series?: bool (every meeting of its series), comment?, propose_start?, propose_end? (tentative or decline: a time proposed instead, Unix seconds)}", "Answers an invitation (the organizer is told; a declined meeting leaves the calendar)"),
    m("v1.calendar.update", P::CalendarWrite, true, "{id, private?, show_as?: free | tentative | busy | oof | workingElsewhere, categories?: [..]}", "Changes an event's private mark, how it shows, its categories"),
    m("v1.calendar.delete", P::CalendarWrite, true, "{id, series?}", "Deletes an event as Outlook does: someone else's meeting is declined, your own meeting with people invited is canceled (they're told); series: true = every meeting of its series"),
    m("v1.trash.summary", P::HeadersRead, false, "{}", "Each account's Trash and what Empty Trash would delete"),
    m("v1.labels.list", P::HeadersRead, false, "{}", "The labels to choose from (name, color)"),
    m("v1.mail.view", P::BodiesRead, false, "{id}", "A message as the window shows it (text, HTML, attachments)"),
    m("v1.mail.source", P::BodiesRead, false, "{id}", "A message's original bytes as text"),
    m("v1.mail.copy_source", P::BodiesRead, false, "{copy}", "One original copy's bytes as text"),
    m("v1.mail.pictures", P::BodiesRead, false, "{id, keep?: bool}", "A message's pictures from the web (fetched by Eudora)"),
    // changing mail
    m("v1.mail.copy", P::Change, true, "{ids, to}", "Copies messages into another folder"),
    m("v1.folders.unread", P::HeadersRead, false, "{}", "Unread messages per folder: [[folder, count]]"),
    m("v1.folders.mark_read", P::Change, true, "{folders: [..]}", "Marks every unread message in these folders read (the servers told in the background)"),
    m("v1.folders.create", P::Change, true, "{account, path}", "Makes a new mailbox, here and on the server"),
    m("v1.folders.rename", P::Change, true, "{folder, name}", "Renames a mailbox (the folders inside go with it), here and on the server; the special folders and the Online Archive can't be"),
    m("v1.folders.delete", P::Delete, true, "{folder}", "Deletes a mailbox as Outlook does: into Deleted Items with its mail and folders, here and on the server (an empty one already in the Trash is removed)"),
    m("v1.rules.run", P::Change, true, "{folder, ids?, dry_run?: bool}", "Runs the rules (those for running by hand) on a folder or some of its messages"),
    m("v1.rules.run_items", P::Change, true, "{items: [[id, folder]..], dry_run?: bool}", "Runs the rules on messages from several folders"),
    m("v1.trash.empty", P::Delete, true, "{accounts: [..]}", "Empty Trash (Microsoft 365: deleted on the server too)"),
    // writing mail
    m("v1.compose.start", P::Compose, false, "{mode: new | reply | replyAll | forward | redirect | again | forwardMany, id?, ids?}", "What a new message, reply, forward… starts with"),
    m("v1.compose.spell", P::Compose, false, "{texts: [..]}", "Spelling mistakes in these texts, with suggestions"),
    m("v1.compose.spell_add", P::Compose, true, "{word}", "Adds a word to the user's dictionary"),
    m("v1.compose.directory", P::Compose, false, "{typed}", "People in the organization's directory (Microsoft 365) whose name or address starts with what's typed"),
    m("v1.compose.complete", P::Compose, false, "{typed}", "People matching what's typed in an address field"),
    m("v1.compose.rebuild_contacts", P::Compose, true, "{}", "Rebuilds the address suggestions from the mail"),
    m("v1.drafts.list", P::Compose, false, "{}", "The saved drafts"),
    m("v1.drafts.save", P::Compose, true, "{id?, draft}", "Saves a draft (new, or over draft id)"),
    m("v1.drafts.delete", P::Compose, true, "{id}", "Deletes a draft"),
    m("v1.outbox.list", P::Send, false, "{}", "The Out mailbox: messages waiting to be sent"),
    m("v1.outbox.unqueue", P::Send, true, "{id}", "Takes a message out of Out, back into Drafts"),
    m("v1.outbox.reschedule", P::Send, true, "{id, send_at}", "When a waiting message goes (unix seconds)"),
    m("v1.outbox.send_now", P::Send, true, "{}", "Send Queued Messages: everything waiting in Out goes now"),
    // the spam filter
    m("v1.spam.state", P::SpamFilter, false, "{}", "Whether arriving mail is sorted, each mailbox's learning, whether Spamhaus is used"),
    m("v1.spam.preview", P::SpamFilter, false, "{items: [[id, folder]..]}", "Where the spam filter would put these messages, and why"),
    m("v1.spam.apply", P::SpamFilter, true, "{chosen: [..]}", "Carries out proposals from v1.spam.preview"),
    m("v1.spam.mark", P::SpamFilter, true, "{items: [[id, folder]..], spam: bool}", "Spam / Not Spam (lists the sender, moves the messages)"),
    m("v1.spam.why", P::SpamFilter, false, "{id}", "Why the spam filter decides what it does for one message"),
    m("v1.spam.rules", P::SpamFilter, false, "{}", "The spam filter's rules in checking order"),
    m("v1.spam.rule_set", P::SpamFilter, true, "{key, settings}", "A rule's settings: off, exceptions, inclusions, mailboxes"),
    m("v1.spam.entry_add", P::SpamFilter, true, "{pattern, kind: block | allow, action?, note?, mailbox?}", "A blacklist or whitelist entry (in one mailbox when given)"),
    m("v1.spam.rule_remove", P::SpamFilter, true, "{key}", "Removes a blacklist / whitelist entry or a \"like this\" rule"),
    m("v1.spam.like_preview", P::SpamFilter, false, "{id}", "A \"messages like this one\" rule from a message, and what it would catch"),
    m("v1.spam.like_add", P::SpamFilter, true, "{name, keys, action, mailbox?}", "Saves a \"like this\" rule (in one mailbox when given)"),
    m("v1.spam.mailboxes", P::SpamFilter, false, "{}", "The mailboxes the spam filter may work in"),
    m("v1.spam.mailboxes_set", P::SpamFilter, true, "{addresses: [..]}", "Chooses the mailboxes it works in"),
    m("v1.spam.languages", P::SpamFilter, false, "{}", "The languages the user reads (others go to Spam), and every language it knows"),
    m("v1.spam.languages_set", P::SpamFilter, true, "{codes: [..]}", "Chooses the languages the user reads (en, it, fr…)"),
    m("v1.spam.arrival", P::SpamFilter, true, "{on: bool}", "Sort arriving mail with the spam filter, or stop"),
    m("v1.spam.learn", P::SpamFilter, true, "{}", "Learns again from each mailbox (minutes)"),
    m("v1.spam.spamhaus_key", P::SpamFilter, true, "{key}", "Keeps a Spamhaus DQS key (empty forgets it); it's never read back"),
    // preferences
    m("v1.settings.get", P::SettingsRead, false, "{key}", "A setting (JSON text)"),
    m("v1.search.scope", P::SettingsRead, false, "{}", "Which mailboxes searches leave out: the switches turned (shared, accounts, account:<id>, folder:<id>) and the folders left out"),
    m("v1.search.scope_set", P::SettingsWrite, true, "{key, searched: true | false | null}", "Turns a search switch (null: follow the level above again)"),
    m("v1.settings.prefix", P::SettingsRead, false, "{prefix}", "Settings whose keys start with prefix"),
    m("v1.signatures.get", P::SettingsRead, false, "{}", "The signatures and each address's default"),
    m("v1.views.accounts", P::SettingsRead, false, "{}", "The mailboxes the combined Views show"),
    m("v1.settings.set", P::SettingsWrite, true, "{key, value}", "Changes a setting (not the API's own)"),
    m("v1.signatures.set", P::SettingsWrite, true, "{signatures}", "Replaces the signatures"),
    m("v1.labels.set_list", P::SettingsWrite, true, "{list: [{name, color}], renamed?: [[old, new]..]}", "The labels to choose from"),
    m("v1.views.accounts_set", P::SettingsWrite, true, "{addresses: [..]}", "Chooses the mailboxes the combined Views show"),
];

fn bad(what: impl Into<String>) -> Refusal {
    Refusal { code: -32602, message: what.into() }
}

fn denied(what: impl Into<String>) -> Refusal {
    Refusal { code: -32001, message: what.into() }
}

fn to<T: Serialize>(v: T) -> R<Value> {
    serde_json::to_value(v).map_err(|e| Refusal::from(Error::from(e)))
}

/// Reading parameters.
struct Params<'a>(&'a Value);

impl Params<'_> {
    fn i64(&self, k: &str) -> R<i64> {
        self.0[k].as_i64().ok_or_else(|| bad(format!("{k}: a number")))
    }
    fn opt_i64(&self, k: &str) -> Option<i64> {
        self.0[k].as_i64()
    }
    fn str(&self, k: &str) -> R<String> {
        self.0[k].as_str().map(str::to_string).ok_or_else(|| bad(format!("{k}: text")))
    }
    fn opt_str(&self, k: &str) -> Option<String> {
        self.0[k].as_str().map(str::to_string).filter(|s| !s.is_empty())
    }
    fn bool(&self, k: &str, default: bool) -> bool {
        self.0[k].as_bool().unwrap_or(default)
    }
    fn ids(&self, k: &str) -> R<Vec<i64>> {
        let list: Vec<i64> = self.0[k].as_array().map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
        if list.is_empty() || list.len() > 10_000 {
            return Err(bad(format!("{k}: 1 to 10,000 numbers")));
        }
        Ok(list)
    }
    fn opt_ids(&self, k: &str) -> Option<Vec<i64>> {
        self.0[k].as_array().map(|a| a.iter().filter_map(Value::as_i64).collect())
    }
    fn de<T: DeserializeOwned>(&self, k: &str) -> R<T> {
        serde_json::from_value(self.0[k].clone()).map_err(|e| bad(format!("{k}: {e}")))
    }
    fn strings(&self, k: &str) -> Vec<String> {
        self.0[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
    }
}

/// A restore never goes into the library this API serves.
fn not_this_library(root: &std::path::Path, dest: &std::path::Path) -> R<()> {
    let real = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if real(root) == real(dest) {
        return Err(bad("dest: that's the library Eudora has open; restore into another folder"));
    }
    Ok(())
}

/// Opens the library for writing, waiting for a download that's letting go of it.
fn open_writer(root: &std::path::Path) -> crate::Result<Library> {
    let mut tries = 0;
    loop {
        match Library::open(root, crate::Access::Write, &crate::Silent) {
            Err(Error::Locked(_)) if tries < 60 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            other => return other,
        }
    }
}

impl Library {
    /// The rest of the window's functions (see `MORE`). `scope`: what the client's grant
    /// reaches; methods that act on the whole library need an unlimited grant.
    pub(crate) fn run_more(&self, host: &dyn Host, client: &Client, scope: &Scope, method: &str, p: &Value) -> R<(Value, String)> {
        let p = Params(p);
        let whole = || -> R<()> {
            if scope.whole() { Ok(()) } else { Err(denied("this works on the whole library: the permission granted is limited to some accounts or folders")) }
        };
        let messages = |ids: &[i64]| -> R<()> {
            for id in ids {
                scope.require(self, *id)?;
            }
            Ok(())
        };
        let folders_ok = |list: &[i64]| -> R<()> {
            match list.iter().find(|f| !scope.folder_ok(**f)) {
                Some(f) => Err(denied(format!("this program may not see folder {f}"))),
                None => Ok(()),
            }
        };
        let accounts_ok = |list: &[i64]| -> R<()> {
            match list.iter().find(|a| !scope.account_id_ok(**a)) {
                Some(a) => Err(denied(format!("this program may not reach account {a}"))),
                None => Ok(()),
            }
        };
        let root = self.root.clone();
        Ok(match method {
            // ── status ──
            "v1.library.info" => {
                let (oldest, newest) = self.date_range()?;
                (json!({ "path": root.display().to_string(), "oldest": oldest, "newest": newest }), "library info".into())
            }
            "v1.library.jobs" => {
                (to(crate::activity::read(&self.root))?, "background jobs".into())
            }
            "v1.library.activity" => {
                let (moves, given_up, last_error) = crate::server_changes::waiting(self)?;
                let (changes, changes_given_up) = crate::server_changes::waiting_marks(self)?;
                let download = crate::graph_sync::read_status(&root);
                (
                    json!({
                        "download": download, "moves_waiting": moves, "changes_waiting": changes, "given_up": given_up + changes_given_up,
                        "last_error": last_error, "out_waiting": self.outbox()?.len(), "offline": crate::graph_sync::is_offline(&root),
                        "pause": crate::graph_sync::read_pause(&root),
                    }),
                    "activity".into(),
                )
            }
            "v1.library.pending" => (to(self.pending_changes()?)?, "pending changes".into()),
            "v1.library.server_changes" => {
                let (waiting, given_up, last_error) = crate::server_changes::waiting(self)?;
                let (account, may_change) = crate::graph::GraphClient::resume_best(None, crate::graph::ALL_SCOPES).map(|c| (c.account.clone(), c.may_change_mail())).unwrap_or_default();
                (json!({ "account": account, "may_change": may_change, "waiting": waiting, "given_up": given_up, "last_error": last_error }), "server changes".into())
            }
            "v1.library.stats" => {
                whole()?;
                (to(self.mail_stats()?)?, "statistics".into())
            }
            "v1.library.pause_state" => (to(crate::graph_sync::read_pause(&root))?, "pause state".into()),
            "v1.sync.state" => {
                use crate::sync_run::{HOME_SETTING, SETTINGS_ACCOUNTS};
                let carriers: Vec<String> = self.setting(SETTINGS_ACCOUNTS)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
                (
                    json!({ "key": crate::sync_envelope::SyncKey::load().map(|k| k.id()), "home": self.setting(HOME_SETTING)?, "carriers": carriers }),
                    "sync state".into(),
                )
            }
            "v1.firewall.status" => (to(crate::firewall::status().map_err(bad)?)?, "firewall status".into()),
            "v1.plugins.list" => (to(self.plugins()?)?, "plugins".into()),
            "v1.updates.status" => (crate::update::status(), "update status".into()),
            "v1.updates.check" => (to(crate::update::check_now(p.bool("force", true), false, |_, _| true)?)?, "looked for an update".into()),
            "v1.updates.install" => {
                let when = p.str("when")?;
                let version = crate::update::answer(&when)?;
                (json!({ "version": version, "when": when }), format!("update {version}: {when}"))
            }
            "v1.updates.settings_set" => {
                let days = u32::try_from(p.i64("every_days")?).map_err(|_| bad("every_days: 0 to 365"))?;
                (to(crate::update::set_choices(p.bool("on_start", true), days)?)?, "update choices changed".into())
            }

            // ── running the library ──
            "v1.library.look" => {
                let folders = p.opt_ids("folders").unwrap_or_default();
                crate::graph_sync::request_look(&root, &folders)?;
                (json!({ "asked": folders.len() }), format!("look at {} folders", folders.len()))
            }
            "v1.library.maintenance" => (
                json!({ "schedule": self.maintenance_schedule(), "last": self.maintenance_last(), "waiting": crate::maintenance::waiting(&root) }),
                "maintenance".into(),
            ),
            "v1.library.speed" => (json!({ "download": crate::speed::read_summary(&root, "download"), "window": crate::speed::read_summary(&root, "window") }), "speed".into()),
            "v1.library.maintain" => {
                crate::maintenance::request(&root)?;
                (json!({ "asked": true }), "maintenance asked for".into())
            }
            "v1.library.maintenance_set" => {
                let s: crate::maintenance::Schedule = p.de("schedule")?;
                if !["off", "daily", "weekly"].contains(&s.when.as_str()) || s.hour > 23 || s.weekday > 6 {
                    return Err(bad("schedule: when off, daily or weekly; hour 0-23; weekday 0-6"));
                }
                self.set_maintenance_schedule(&s)?;
                (json!({ "set": true }), "maintenance schedule".into())
            }
            "v1.library.check_mail" => {
                crate::graph_sync::request_check(&root)?;
                (json!({ "asked": true }), "check mail".into())
            }
            "v1.library.pause" => {
                use crate::graph_sync::Pause;
                let mode = p.str("mode")?;
                let pause = match mode.as_str() {
                    "old" => Pause::Old,
                    "all" => Pause::All,
                    "none" => Pause::None,
                    _ => return Err(bad("mode: none, old or all")),
                };
                crate::graph_sync::set_pause(&root, pause)?;
                (json!({ "pause": pause }), format!("pause {mode}"))
            }
            "v1.library.job_log" => {
                let job = p.str("job")?;
                let before = p.0["before"].as_i64();
                let runs = p.0["runs"].as_u64().unwrap_or(1).clamp(1, 50) as usize;
                (to(self.job_log(&job, before, runs)?)?, format!("details of {job}"))
            }
            "v1.library.job_pause" => {
                let key = p.str("key")?;
                let paused = p.bool("paused", true);
                crate::activity::set_job_paused(&root, &key, paused)?;
                (json!({ "key": key, "paused": paused }), format!("{} {key}", if paused { "pause" } else { "resume" }))
            }
            "v1.library.offline" => {
                if let Some(on) = p.0["on"].as_bool() {
                    crate::graph_sync::set_offline(&root, on)?;
                }
                let now_offline = crate::graph_sync::is_offline(&root);
                (json!({ "offline": now_offline }), format!("offline: {now_offline}"))
            }
            "v1.library.background" => {
                let set = (!p.0["options"].is_null()).then(|| &p.0["options"]);
                (host.background(set)?, if set.is_some() { "background download options changed".into() } else { "background download state".into() })
            }
            "v1.library.backup_info" => (to(crate::backup::sizes(&root)?)?, "backup sizes".into()),
            "v1.library.backup" => {
                whole()?;
                let dest = PathBuf::from(p.str("dest")?.trim());
                let options = crate::backup::BackupOptions {
                    dest: dest.clone(),
                    password: p.opt_str("password"),
                    indexes: p.bool("indexes", false),
                    // never through the API: passwords stay on this computer
                    passwords: false,
                    agreed: String::new(),
                    part_size: p.0["part_gb"].as_f64().filter(|g| *g > 0.0).map(|g| (g * 1e9) as u64),
                };
                if let Some(dir) = dest.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| bad(format!("can't make {}: {e}", dir.display())))?;
                }
                let r = host.exclusive(&mut || {
                    let mut tries = 0;
                    loop {
                        match crate::backup::back_up(&root, &options, &crate::Silent) {
                            Err(Error::Other(e)) if e.contains("being written") && tries < 60 => {
                                tries += 1;
                                std::thread::sleep(std::time::Duration::from_millis(500));
                            }
                            other => return other.and_then(|r| Ok(serde_json::to_value(r)?)),
                        }
                    }
                })?;
                (r, format!("backed up the library to {}", dest.display()))
            }
            "v1.library.restore_describe" => {
                let (h, parts) = crate::backup::describe(std::path::Path::new(p.str("path")?.trim()))?;
                (json!({ "header": h, "parts": parts.len() }), "described a backup".into())
            }
            "v1.library.restore_destination" => {
                let dest = PathBuf::from(p.str("dest")?.trim());
                not_this_library(&root, &dest)?;
                (to(crate::backup::destination(&dest)?)?, format!("looked at {}", dest.display()))
            }
            "v1.library.restore" => {
                let backup = PathBuf::from(p.str("path")?.trim());
                let dest = match p.opt_str("dest") {
                    Some(d) => PathBuf::from(d.trim()),
                    None => crate::library::restore_target().ok_or_else(|| bad("dest: this computer has no usual place for the library; name a folder"))?,
                };
                not_this_library(&root, &dest)?;
                let replace = p.0["replace"].as_bool() == Some(true);
                let (dest, r) = crate::library::restore_as_library(&backup, Some(&dest), p.opt_str("password").as_deref(), replace, &crate::Silent)?;
                (json!({ "report": r, "library": dest }), format!("restored a backup into {}", dest.display()))
            }
            "v1.library.import" => {
                let paths: Vec<PathBuf> = p.strings("paths").into_iter().map(|s| PathBuf::from(s.trim())).collect();
                if paths.is_empty() {
                    return Err(bad("paths: what to import"));
                }
                let options = crate::ImportOptions { destination: p.opt_str("into"), account: p.opt_str("account"), merge_account: p.0["merge"].as_bool(), ..Default::default() };
                let shown = paths.iter().map(|x| x.display().to_string()).collect::<Vec<_>>().join(", ");
                let r = host.exclusive(&mut || {
                    let mut l = open_writer(&root)?;
                    let r = crate::import(&mut l, &paths, &options, &crate::Silent);
                    l.close(&crate::Silent)?;
                    Ok(serde_json::to_value(r?)?)
                })?;
                (r, format!("imported {shown}"))
            }
            "v1.library.optimize" => {
                let vacuum = p.bool("vacuum", false);
                let r = host.exclusive(&mut || {
                    let l = open_writer(&root)?;
                    let r = l.optimize(vacuum, &crate::Silent);
                    l.close(&crate::Silent)?;
                    Ok(serde_json::to_value(r?)?)
                })?;
                (r, "optimized the library".into())
            }
            "v1.library.old_attachments" => {
                let folders: Vec<PathBuf> = p.strings("folders").into_iter().map(PathBuf::from).collect();
                let r = host.exclusive(&mut || {
                    let l = open_writer(&root)?;
                    let r = l.find_old_attachments(&folders, &crate::Silent);
                    l.close(&crate::Silent)?;
                    Ok(serde_json::to_value(r?)?)
                })?;
                (r, "looked for old attachments".into())
            }
            "v1.sync.setup" => {
                use crate::sync_envelope::SyncKey;
                if SyncKey::load().is_some() {
                    return Err(bad("this computer already syncs"));
                }
                let key = SyncKey::generate()?;
                key.store()?;
                (json!({ "key": key.id(), "note": "the recovery phrase is shown only in Eudora's window (Settings ▸ Downloading and sync)" }), "set up sync".into())
            }
            "v1.sync.join" => {
                let key = crate::sync_envelope::SyncKey::from_phrase(&p.str("phrase")?)?;
                key.store()?;
                (json!({ "key": key.id() }), "joined sync".into())
            }
            "v1.sync.carriers" => {
                use crate::sync_run::{HOME_SETTING, SETTINGS_ACCOUNTS};
                self.set_setting(HOME_SETTING, p.str("home")?.trim())?;
                self.set_setting(SETTINGS_ACCOUNTS, &serde_json::to_string(&p.strings("carriers")).map_err(Error::from)?)?;
                (json!({}), "sync carriers changed".into())
            }
            "v1.sync.now" => {
                if p.bool("repair", false) {
                    std::fs::create_dir_all(root.join("status")).and_then(|_| std::fs::write(root.join("status").join("sync-repair"), b"")).map_err(|e| bad(e.to_string()))?;
                }
                crate::graph_sync::request_check(&root)?;
                (json!({ "asked": true }), "sync now".into())
            }
            "v1.firewall.set" => {
                let port = p.i64("port")?;
                let allow = p.bool("allow", true);
                let status = crate::firewall::set(u16::try_from(port).map_err(|_| bad("port: 1 to 65535"))?, allow).map_err(bad)?;
                (to(status)?, format!("firewall: port {port} {}", if allow { "opened" } else { "closed" }))
            }
            "v1.plugins.install" => {
                let path = p.str("path")?;
                (to(self.plugin_install(std::path::Path::new(path.trim()))?)?, format!("installed a plugin from {path}"))
            }
            "v1.plugins.enable" => {
                let (id, on) = (p.str("id")?, p.bool("on", true));
                self.plugin_enable(&id, on)?;
                (json!({}), format!("plugin {id} {}", if on { "on" } else { "off" }))
            }
            "v1.plugins.remove" => {
                let id = p.str("id")?;
                self.plugin_remove(&id)?;
                (json!({}), format!("removed plugin {id}"))
            }
            "v1.plugins.run" => {
                let (id, command) = (p.str("id")?, p.str("command")?);
                let context = if p.0["context"].is_null() { json!({}) } else { p.0["context"].clone() };
                (to(self.plugin_run(&id, &command, &context)?)?, format!("ran plugin {id} {command}"))
            }
            "v1.rules.import_classic" => {
                let (path, account) = (p.str("path")?, p.str("account")?);
                let bytes = std::fs::read(&path).map_err(|e| bad(format!("{path}: {e}")))?;
                let rules = crate::rules::import_classic(&crate::parse::decode_8bit(&bytes), &account)?;
                let n = rules.len();
                (to(rules)?, format!("read {n} classic rules from {path}"))
            }

            // ── accounts ──
            "v1.accounts.identities" => (to(self.identities()?)?, "identities".into()),
            "v1.accounts.check_every" => (
                json!({ "accounts": crate::graph_sync::check_intervals(self), "default": crate::graph_sync::DEFAULT_CHECK_MINUTES, "choices": crate::graph_sync::CHECK_CHOICES }),
                "check intervals".into(),
            ),
            "v1.accounts.check_every_set" => {
                whole()?;
                let (address, minutes) = (p.str("address")?, p.i64("minutes")?);
                let minutes = u32::try_from(minutes).map_err(|_| bad("minutes: 1, 5, 10, 15, 30 or 60"))?;
                crate::graph_sync::set_check_minutes(self, &address, minutes)?;
                (json!({}), format!("{address} checked every {minutes} minutes"))
            }
            "v1.accounts.download_period" => {
                let chosen = crate::graph_sync::download_period(self);
                (json!({ "chosen": chosen.is_some(), "days": chosen.flatten(), "mb": crate::graph_sync::download_space_mb(self) }), "download period".into())
            }
            "v1.accounts.download_period_set" => {
                whole()?;
                let days = match p.0.get("days") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(v) => Some(v.as_u64().and_then(|d| u32::try_from(d).ok()).ok_or_else(|| bad("days: a number of days, or null for everything"))?),
                };
                let mb = p.0.get("mb").and_then(|v| v.as_u64());
                if let Some(mb) = mb {
                    crate::graph_sync::set_download_space(self, mb)?;
                    (json!({}), format!("download up to {mb} MB"))
                } else {
                    crate::graph_sync::set_download_period(self, days)?;
                    (json!({}), match days { Some(d) => format!("download the last {d} days"), None => "download everything".into() })
                }
            }
            "v1.accounts.keep_preview" => {
                whole()?;
                let days = p.0.get("days").and_then(|v| v.as_u64()).and_then(|d| u32::try_from(d).ok()).filter(|d| *d > 0).ok_or_else(|| bad("days: a number of days"))?;
                (to(self.keep_preview(days)?)?, format!("what keeping {days} days would mean"))
            }
            "v1.accounts.sign_ins" => (to(self.sign_ins()?.into_iter().map(|s| s.what).collect::<Vec<_>>())?, "sign-ins listed".into()),
            "v1.accounts.missing_sign_ins" => (to(self.missing_sign_ins()?)?, "missing sign-ins listed".into()),
            "v1.accounts.set_name" => {
                let id = p.i64("id")?;
                accounts_ok(&[id])?;
                let name = p.str("name")?;
                self.set_account_label(id, Some(&name))?;
                (json!({}), format!("account {id} named"))
            }
            "v1.accounts.readdress" => {
                let id = p.i64("id")?;
                accounts_ok(&[id])?;
                let old = self.set_account_address(id, &p.str("address")?)?;
                (json!({ "was": old }), format!("account {id} readdressed"))
            }
            "v1.accounts.imap_list" => (to(self.imap_accounts()?)?, "IMAP / POP3 accounts".into()),
            "v1.accounts.mail_host" => {
                let address = p.str("address")?;
                (to(crate::mail_host::mail_host(&address))?, format!("who hosts {address}"))
            }
            "v1.accounts.imap_test" => {
                let config: crate::imap_sync::ImapConfig = p.de("config")?;
                let r = crate::imap_sync::test(&config, &p.str("password")?, &p.opt_str("smtp_password").unwrap_or_default())?;
                (to(r)?, format!("tested {}", config.address))
            }
            "v1.accounts.imap_save" => {
                let config: crate::imap_sync::ImapConfig = p.de("config")?;
                let id = self.save_imap_account(&config, p.opt_str("password").as_deref(), p.opt_str("smtp_password").as_deref())?;
                (json!({ "account": id }), format!("saved account {}", config.address))
            }
            "v1.accounts.find_shared" => {
                let mut client = crate::graph::GraphClient::resume(None, crate::graph::ALL_SCOPES)?;
                (to(crate::graph_sync::find_mailboxes(&mut client, &crate::Silent)?)?, "found shared mailboxes".into())
            }
            "v1.accounts.add_shared" => {
                let list = p.strings("addresses");
                for address in &list {
                    self.add_shared_mailbox(address)?;
                }
                (json!({ "added": list.len() }), format!("added {}", list.join(", ")))
            }
            "v1.accounts.microsoft_sign_in" => {
                let client = crate::graph::GraphClient::sign_in_any(&|url| { let _ = crate::graph::open_browser(url); }, p.opt_str("address").as_deref())?;
                let _ = crate::server_changes::retry_given_up(self);
                self.add_microsoft_account(&client.account, &client.display_name)?;
                (json!({ "account": client.account }), "signed in to Microsoft 365".into())
            }
            "v1.accounts.google_available" => (json!(crate::google::available()), "Google sign-in available".into()),
            "v1.accounts.google_sign_in" => {
                let address = p.opt_str("address");
                let client = crate::google::GoogleClient::sign_in(&|url| { let _ = crate::graph::open_browser(url); }, address.as_deref())?;
                self.add_google_account(&client.account)?;
                let _ = crate::graph_sync::request_check(&root);
                (json!({ "account": client.account }), format!("signed in to Google as {}", client.account))
            }
            "v1.accounts.forget_sign_ins" => {
                let done = self.forget_sign_ins()?;
                let n = done.len();
                (to(done)?, format!("forgot {n} sign-ins"))
            }

            // ── reading mail ──
            "v1.mail.page" | "v1.mail.count" | "v1.mail.ids" | "v1.mail.sorted" => {
                let list_folders = p.opt_ids("folders").unwrap_or_default();
                let view = p.opt_str("view");
                let hide_sent = p.bool("hide_sent", false);
                let accounts = match p.opt_ids("accounts") {
                    Some(a) => a,
                    None if view.is_some() => self.combined_view_accounts()?,
                    None => Vec::new(),
                };
                if list_folders.is_empty() || view.is_some() {
                    // All Mail or a view: across accounts
                    if view.is_some() { accounts_ok(&accounts)? } else { whole()? }
                } else {
                    folders_ok(&list_folders)?;
                }
                let hide: Vec<i64> = if list_folders.is_empty() && hide_sent && view.is_none() { self.address_ids(&self.own_addresses()?)? } else { Vec::new() };
                match method {
                    "v1.mail.count" => {
                        let n = match &view {
                            Some(v) => self.count_view(v, &accounts)?,
                            None if !hide.is_empty() => self.count_all_except_from(&hide)?,
                            None => self.count(&list_folders)?,
                        };
                        (json!({ "count": n }), format!("counted {n}"))
                    }
                    "v1.mail.ids" => {
                        let ids = match &view {
                            Some(v) => self.view_ids(v, &accounts)?,
                            None => self.list_ids(&list_folders, &hide)?,
                        };
                        let n = ids.len();
                        (to(ids)?, format!("{n} message numbers"))
                    }
                    _ => {
                        let limit = p.opt_i64("limit").unwrap_or(100).clamp(1, 1000) as usize;
                        let after = match (p.0["after"]["date"].as_i64(), p.0["after"]["id"].as_i64()) {
                            (Some(date), Some(id)) => Some(PageCursor { date, id }),
                            _ => None,
                        };
                        let rows = if method == "v1.mail.sorted" {
                            // sorted by a column: the whole list, then `offset` on
                            let all: Vec<i64> = match &view {
                                Some(v) => self.view_ids(v, &accounts)?,
                                None => self.list_ids(&list_folders, &hide)?,
                            }
                            .into_iter()
                            .map(|(id, _)| id)
                            .collect();
                            let sorted = self.sort_ids(&all, &p.str("sort")?, p.bool("ascending", true))?;
                            let offset = (p.opt_i64("offset").unwrap_or(0).max(0) as usize).min(sorted.len());
                            self.rows(&sorted[offset..(offset + limit).min(sorted.len())])?
                        } else {
                            match &view {
                                Some(v) => self.list_view(v, &accounts, after, limit)?,
                                None if !hide.is_empty() => self.list_all_except_from(&hide, after, limit)?,
                                None => self.list(&list_folders, after, limit)?,
                            }
                        };
                        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
                        let recipients = self.recipients_of(&ids)?;
                        let out: Vec<Value> = rows
                            .iter()
                            .zip(recipients)
                            .map(|(r, (to_, cc))| {
                                json!({
                                    "id": r.id, "date": r.date, "from": r.from, "to": to_, "cc": cc, "subject": r.subject, "snippet": r.snippet,
                                    "size": r.size, "attachments": r.attachments, "message_id": r.message_id, "list_id": r.list_id,
                                    "labels": r.labels, "unread": r.flags & crate::marks::UNREAD != 0, "priority": crate::marks::priority_of(r.flags),
                                })
                            })
                            .collect();
                        let n = out.len();
                        (Value::Array(out), format!("listed {n} messages"))
                    }
                }
            }
            "v1.mail.header_values" => {
                let ids = p.ids("ids")?;
                messages(&ids)?;
                let names = p.strings("names");
                (to(self.header_values(&ids, &names)?)?, format!("{} headers of {} messages", names.len(), ids.len()))
            }
            "v1.mail.copies" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                (to(self.copies_of(id)?)?, format!("copies of message {id}"))
            }
            "v1.mail.links" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                (to(self.links_of(id)?)?, format!("links of message {id}"))
            }
            "v1.views.list" | "v1.views.groups" => {
                let accounts = match p.opt_ids("accounts") {
                    Some(a) => a,
                    None => self.combined_view_accounts()?,
                };
                accounts_ok(&accounts)?;
                if method == "v1.views.list" {
                    (to(self.views(&accounts)?)?, "views".into())
                } else {
                    let view = p.str("view")?;
                    (to(self.view_groups(&view, &accounts)?)?, format!("groups of {view}"))
                }
            }
            "v1.views.why" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                (to(self.view_scores_of(id)?)?, format!("views' points for message {id}"))
            }
            "v1.subscriptions.list" => {
                whole()?;
                (to(self.subscriptions()?)?, "subscriptions".into())
            }
            "v1.subscriptions.unsubscribe" => {
                whole()?;
                let key = p.str("key")?;
                (to(self.unsubscribe(&key, p.i64("newest")?)?)?, format!("unsubscribed from {key}"))
            }
            "v1.mail.attachment_save" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                let path = self.save_attachment(id, &p.str("part")?, std::path::Path::new(p.str("dir")?.trim()))?;
                (json!({ "path": path }), format!("saved an attachment of message {id}"))
            }
            "v1.mail.attachments_save_all" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                let (saved, missing) = self.save_all_attachments(id, std::path::Path::new(p.str("dir")?.trim()))?;
                (json!({ "saved": saved, "missing": missing }), format!("saved {} attachments of message {id}", saved.len()))
            }
            "v1.mail.save_as" => {
                let ids = p.ids("ids")?;
                messages(&ids)?;
                let how = match p.0["format"].as_str().unwrap_or("msg") {
                    "msg" => crate::msg::SaveAs::Msg,
                    "eml" => crate::msg::SaveAs::Eml,
                    other => return Err(bad(format!("format: msg or eml, not {other}"))),
                };
                let saved = self.save_messages(&ids, how, std::path::Path::new(p.str("dir")?.trim()))?;
                (json!({ "saved": saved }), format!("saved {} messages as files", saved.len()))
            }
            "v1.contacts.list" | "v1.contacts.export" => {
                let account = p.0["account"].as_i64();
                match account {
                    Some(a) => accounts_ok(&[a])?,
                    None => whole()?,
                }
                if method == "v1.contacts.list" {
                    let query = p.opt_str("query").unwrap_or_default();
                    (to(self.cards(&query, account)?)?, "address book".into())
                } else {
                    let path = p.str("path")?;
                    let n = self.export_cards(std::path::Path::new(path.trim()), account)?;
                    (json!({ "contacts": n }), format!("exported {n} contacts to {path}"))
                }
            }
            "v1.calendar.calendars" => {
                whole()?;
                (to(self.calendars_list()?)?, "calendars".into())
            }
            "v1.calendar.events" => {
                whole()?;
                let (from, to_) = (p.i64("from")?, p.i64("to")?);
                (to(self.events_between(from, to_)?)?, format!("events from {from} to {to_}"))
            }
            "v1.calendar.search" => {
                whole()?;
                let words = p.str("words")?;
                let limit = p.0["limit"].as_u64().unwrap_or(200).min(2000) as usize;
                (to(self.search_events(&words, limit)?)?, format!("events matching {words}"))
            }
            "v1.calendar.get" => {
                let id = p.i64("id")?;
                let v = self.event_get(id)?;
                accounts_ok(&[self.calendar_account(v.calendar)?])?;
                (to(v)?, format!("event {id}"))
            }
            "v1.calendar.reservations" => {
                whole()?;
                (to(self.reservations()?)?, "reservations".into())
            }
            "v1.calendar.invite_spam" => {
                whole()?;
                let event = p.i64("event")?;
                let said = self.invite_spam(event, p.bool("series", false))?;
                (to(&said)?, format!("invitation {event} reported as spam"))
            }
            "v1.calendar.invites" => {
                whole()?;
                let list = self.invites()?;
                let n = list.len();
                (to(list)?, format!("{n} invitations waiting"))
            }
            "v1.calendar.invitation" => {
                // (the message's invite file: reading it needs its text)
                let id = p.i64("id")?;
                whole()?;
                (to(self.invitation(id)?)?, format!("invitation in message {id}"))
            }
            "v1.calendar.invitation_add" => {
                let id = p.i64("id")?;
                whole()?;
                let calendar = p.0.get("calendar").and_then(|c| c.as_i64());
                (to(self.invitation_add(id, calendar)?)?, format!("added message {id}'s meeting to a calendar"))
            }
            "v1.calendar.reservation_off" => {
                whole()?;
                let key = p.str("key")?;
                (json!({ "done": self.remove_reservation_event(&key)? }), format!("reservation {key} off the calendar"))
            }
            "v1.calendar.reservation_hide" => {
                whole()?;
                let key = p.str("key")?;
                self.hide_reservation(&key)?;
                (json!({}), format!("reservation {key} removed from Trips"))
            }
            "v1.calendar.trips_from_message" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                (json!({ "found": self.trips_from_message(id)? }), format!("trips read from message {id}"))
            }
            "v1.calendar.reservations_answer" => {
                whole()?;
                let n = self.answer_reservations(&p.strings("add"), &p.strings("decline"))?;
                (json!({ "added": n }), format!("{n} reservations added to the calendar"))
            }
            "v1.todo.list" => {
                whole()?;
                (to(self.todos()?)?, "To Do".into())
            }
            "v1.todo.lists" => {
                whole()?;
                (to(self.todo_lists()?)?, "To Do lists".into())
            }
            "v1.todo.list_color" => {
                whole()?;
                let id = p.str("id")?;
                let color = p.0["color"].as_str().map(str::to_string);
                self.set_todo_list_color(&id, color.as_deref())?;
                (json!({}), format!("To Do list {id} color {}", color.as_deref().unwrap_or("its first")))
            }
            "v1.todo.list_shown" => {
                whole()?;
                let (id, shown) = (p.str("id")?, p.bool("shown", true));
                self.set_todo_list_shown(&id, shown)?;
                (json!({}), format!("To Do list {id} {}", if shown { "shown" } else { "hidden" }))
            }
            "v1.todo.add" => {
                whole()?;
                let message = p.0["message"].as_i64();
                if let Some(m) = message {
                    messages(&[m])?;
                }
                let id = self.todo_add(&p.str("title")?, &p.opt_str("notes").unwrap_or_default(), p.0["due"].as_i64(), message)?;
                (json!({ "id": id }), format!("task {id} added"))
            }
            "v1.todo.update" => {
                whole()?;
                let id = p.i64("id")?;
                let due = p.0.get("due").map(|d| d.as_i64());
                self.todo_update(id, p.opt_str("title").as_deref(), p.opt_str("notes").as_deref(), due, p.0["done"].as_bool(), p.0["important"].as_bool())?;
                (json!({}), format!("task {id} changed"))
            }
            "v1.todo.delete" => {
                whole()?;
                let id = p.i64("id")?;
                self.todo_delete(id)?;
                (json!({}), format!("task {id} deleted"))
            }
            "v1.todo.flag" => {
                let ids = p.ids("ids")?;
                messages(&ids)?;
                let on = p.0["on"].as_bool().unwrap_or(true);
                let made = self.flag_messages(&ids, on)?;
                (json!({ "tasks": made }), format!("{} {} messages", if on { "flagged" } else { "unflagged" }, ids.len()))
            }
            "v1.todo.twos" => {
                whole()?;
                let id = p.i64("id")?;
                self.send_todo_to_twos(id)?;
                (json!({}), format!("task {id} sent to Twos"))
            }
            "v1.calendar.respond" | "v1.calendar.update" | "v1.calendar.delete" => {
                let id = p.i64("id")?;
                accounts_ok(&[self.calendar_account(self.event_get(id)?.calendar)?])?;
                match method {
                    "v1.calendar.respond" => {
                        let answer = p.str("answer")?;
                        let proposed = p.opt_i64("propose_start").zip(p.opt_i64("propose_end"));
                        self.event_respond_with(id, &answer, p.bool("series", false), &p.opt_str("comment").unwrap_or_default(), proposed)?;
                        (json!({}), format!("answered {answer} to event {id}"))
                    }
                    "v1.calendar.update" => {
                        let categories: Option<Vec<String>> = p.0.get("categories").and_then(|c| c.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
                        self.event_update(id, p.0["private"].as_bool(), p.0["show_as"].as_str(), categories.as_deref())?;
                        (json!({}), format!("changed event {id}"))
                    }
                    _ => (json!({ "done": self.event_delete_as(id, p.bool("series", false))? }), format!("deleted event {id}")),
                }
            }
            "v1.calendar.create" => {
                let e: crate::meetings::NewEvent = serde_json::from_value(p.0.clone()).map_err(crate::error::Error::from)?;
                accounts_ok(&[self.calendar_account(e.calendar)?])?;
                let id = self.new_appointment(&e)?;
                (json!({ "id": id }), format!("appointment {id} made"))
            }
            "v1.calendar.appointment" => {
                let id = p.i64("id")?;
                let a = self.appointment(id)?;
                accounts_ok(&[self.calendar_account(a.calendar)?])?;
                (to(a)?, format!("appointment {id}"))
            }
            "v1.calendar.edit" => {
                let id = p.i64("id")?;
                let e: crate::meetings::NewEvent = serde_json::from_value(p.0["event"].clone()).map_err(crate::error::Error::from)?;
                accounts_ok(&[self.calendar_account(self.appointment(id)?.calendar)?])?;
                self.edit_appointment(id, &e, p.bool("body_changed", false))?;
                (json!({}), format!("appointment {id} changed"))
            }
            "v1.calendar.ics" => {
                let id = p.i64("id")?;
                let a = self.appointment(id)?;
                accounts_ok(&[self.calendar_account(a.calendar)?])?;
                let text = a.body.as_deref().map(|h| mail_parser::decoders::html::html_to_text(h).to_string()).unwrap_or_else(|| a.event.preview.clone());
                (json!({ "ics": crate::meetings::ics(&a.event, &text, crate::library::now()) }), format!("appointment {id} as .ics"))
            }
            "v1.calendar.conference_options" => {
                let calendar = p.i64("calendar")?;
                accounts_ok(&[self.calendar_account(calendar)?])?;
                (to(self.conference_options(calendar)?)?, format!("online meetings for calendar {calendar}"))
            }
            "v1.calendar.places" => {
                let text = p.str("text")?;
                (to(crate::meetings::suggest_places(&text)?)?, "places suggested".into())
            }
            "v1.calendar.color" => {
                let id = p.i64("id")?;
                accounts_ok(&[self.calendar_account(id)?])?;
                let color = p.0["color"].as_str().map(str::to_string);
                self.set_calendar_color(id, color.as_deref())?;
                (json!({}), format!("calendar {id} color {}", color.as_deref().unwrap_or("its own")))
            }
            "v1.calendar.shown" => {
                let id = p.i64("id")?;
                accounts_ok(&[self.calendar_account(id)?])?;
                let shown = p.0["shown"].as_bool().unwrap_or(true);
                self.set_calendar_shown(id, shown)?;
                (json!({}), format!("calendar {id} {}", if shown { "shown" } else { "hidden" }))
            }
            "v1.contacts.share" => {
                let id = p.i64("id")?;
                accounts_ok(&[self.card(id)?.account])?;
                (json!({ "draft": self.share_card_draft(id)? }), format!("a draft sharing contact {id}"))
            }
            "v1.contacts.origin" | "v1.contacts.update" | "v1.contacts.delete" => {
                let id = p.i64("id")?;
                let row = self.card(id)?;
                accounts_ok(&[row.account])?;
                match method {
                    "v1.contacts.origin" => (to(self.card_origin(id)?)?, format!("where contact {id} came from")),
                    "v1.contacts.update" => {
                        let card: crate::cards::Card = p.de("card")?;
                        self.update_card(id, &card)?;
                        (json!({}), format!("changed contact {id}"))
                    }
                    _ => {
                        self.delete_card(id)?;
                        (json!({}), format!("deleted contact {id}"))
                    }
                }
            }
            "v1.contacts.create" => {
                let account = p.i64("account")?;
                accounts_ok(&[account])?;
                let card: crate::cards::Card = p.de("card")?;
                let id = self.create_card(account, &p.opt_str("book").unwrap_or_default(), &card)?;
                (json!({ "id": id }), format!("added contact {id}"))
            }
            "v1.contacts.counts" => {
                let list: Vec<_> = self.card_counts()?.into_iter().filter(|(account, ..)| scope.account_id_ok(*account)).collect();
                (to(list)?, "address book counts".into())
            }
            "v1.contacts.get" => {
                let row = self.card(p.i64("id")?)?;
                accounts_ok(&[row.account])?;
                (to(row)?, "a contact".into())
            }
            "v1.trash.summary" => {
                let list: Vec<_> = self.trash_summary()?.into_iter().filter(|t| scope.account_id_ok(t.account)).collect();
                (to(list)?, "trash summary".into())
            }
            "v1.labels.list" => (to(self.label_list()?)?, "labels".into()),
            "v1.mail.view" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                (to(self.message_view(id)?)?, format!("message {id} as shown"))
            }
            "v1.mail.source" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                let raw = self.raw_message(id)?;
                (json!({ "source": crate::parse::decode_8bit(&raw) }), format!("source of message {id}"))
            }
            "v1.mail.copy_source" => {
                let copy = p.i64("copy")?;
                let message: i64 = self.conn.query_row("SELECT message FROM copies WHERE id = ?1", [copy], |r| r.get(0)).map_err(Error::from)?;
                messages(&[message])?;
                let raw = self.copy_bytes(copy)?;
                (json!({ "source": crate::parse::decode_8bit(&raw), "size": raw.len() }), format!("copy {copy} of message {message}"))
            }
            "v1.mail.pictures" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                // only the pictures the message itself names: never an address the program chose
                let view = self.message_view(id)?;
                // a fraud's pictures are never fetched (they tell the sender it was opened)
                if let Some(w) = &view.warning {
                    return Err(Refusal::new(-32000, format!("not fetched: {w}")));
                }
                let urls = view.web_pictures;
                let dir = p.bool("keep", false).then(|| self.images_dir());
                (to(crate::remote_images::fetch(&urls, dir.as_deref()))?, format!("pictures of message {id} ({} from the web)", urls.len()))
            }

            // ── changing mail ──
            "v1.folders.unread" => {
                whole()?;
                (to(self.unread_by_folder()?)?, "unread per folder".into())
            }
            "v1.folders.mark_read" => {
                let folders = p.ids("folders")?;
                folders_ok(&folders)?;
                let ids = self.unread_in(&folders)?;
                let mut n = 0;
                for chunk in ids.chunks(2_000) {
                    n += self.set_status(chunk, crate::marks::Status::Read)?;
                }
                (json!({ "marked": n }), format!("{n} marked read in {} folders", folders.len()))
            }
            "v1.folders.create" => {
                let account = p.i64("account")?;
                accounts_ok(&[account])?;
                let path = p.str("path")?;
                let id = self.create_folder(account, &path)?;
                (json!({ "id": id }), format!("made the mailbox {path}"))
            }
            "v1.folders.rename" => {
                let folder = p.i64("folder")?;
                folders_ok(&[folder])?;
                let name = p.str("name")?;
                let to = self.rename_folder(folder, &name)?;
                (json!({ "path": to }), format!("renamed a mailbox to {to}"))
            }
            "v1.folders.delete" => {
                let folder = p.i64("folder")?;
                folders_ok(&[folder])?;
                let to = self.delete_folder(folder)?;
                (json!({ "path": to }), format!("deleted mailbox {folder}"))
            }
            "v1.mail.copy" => {
                let ids = p.ids("ids")?;
                messages(&ids)?;
                let to_folder = p.i64("to")?;
                folders_ok(&[to_folder])?;
                let n = self.copy_messages(&ids, to_folder)?;
                (json!({ "copied": n }), format!("copied {n} messages to folder {to_folder}"))
            }
            "v1.rules.run" => {
                let folder = p.i64("folder")?;
                folders_ok(&[folder])?;
                let ids = p.opt_ids("ids");
                if let Some(ids) = &ids {
                    messages(ids)?;
                }
                let dry = p.bool("dry_run", false);
                let r = self.run_rules(folder, ids.as_deref(), dry, &crate::Silent)?;
                let said = format!("rules on folder {folder}{}: {} moved", if dry { " (dry run)" } else { "" }, r.moved);
                (to(r)?, said)
            }
            "v1.rules.run_items" => {
                let items: Vec<(i64, i64)> = p.de("items")?;
                let dry = p.bool("dry_run", false);
                let mut by_folder: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
                for (id, folder) in &items {
                    scope.require(self, *id)?;
                    folders_ok(&[*folder])?;
                    by_folder.entry(*folder).or_default().push(*id);
                }
                let mut total = crate::rules::RunReport::default();
                for (folder, ids) in by_folder {
                    total.add(self.run_rules(folder, Some(&ids), dry, &crate::Silent)?);
                }
                let said = format!("rules on {} messages{}: {} moved", items.len(), if dry { " (dry run)" } else { "" }, total.moved);
                (to(total)?, said)
            }
            "v1.trash.empty" => {
                let accounts = p.ids("accounts")?;
                accounts_ok(&accounts)?;
                let n = self.empty_trash(&accounts)?;
                (json!({ "emptied": n }), format!("emptied the Trash: {n} messages"))
            }

            // ── writing mail ──
            "v1.compose.start" => {
                let mode = p.str("mode")?;
                let id = p.opt_i64("id");
                let ids = p.opt_ids("ids").unwrap_or_default();
                let quoted: Vec<i64> = id.into_iter().chain(ids.iter().copied()).collect();
                if !quoted.is_empty() {
                    // a reply or forward holds the message it answers: reading it is needed too
                    if !client.may(Permission::BodiesRead) {
                        return Err(denied("a reply or forward holds the original's text: this program may not read message text"));
                    }
                    messages(&quoted)?;
                }
                (to(self.compose_start(&mode, id, &ids)?)?, format!("started a {mode}"))
            }
            "v1.compose.spell" => {
                let texts = p.strings("texts");
                (to(crate::spell::check(&texts)?)?, format!("checked spelling of {} texts", texts.len()))
            }
            "v1.compose.spell_add" => {
                let word = p.str("word")?;
                crate::spell::add_word(&word)?;
                (json!({}), format!("added \"{word}\" to the dictionary"))
            }
            "v1.compose.directory" => {
                let typed = p.str("typed")?;
                let mut client = crate::graph::GraphClient::resume_best(None, crate::graph::ALL_SCOPES)?;
                let found = client.search_directory(&typed, 8)?;
                (to(found.into_iter().map(|(name, address)| json!({ "name": name, "address": address })).collect::<Vec<_>>())?, "directory suggestions".into())
            }
            "v1.compose.complete" => {
                let typed = p.str("typed")?;
                let c: Value = self.setting("completion")?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or(json!({}));
                let found = self.complete_address(&typed, c["sent"].as_bool().unwrap_or(true), c["received"].as_bool().unwrap_or(true), 8)?;
                (to(found.into_iter().map(|(name, address)| json!({ "name": name, "address": address })).collect::<Vec<_>>())?, "address suggestions".into())
            }
            "v1.compose.rebuild_contacts" => {
                let mine: Vec<String> = self.identities()?.into_iter().map(|i| i.address).collect();
                let n = self.rebuild_contacts(&mine)?;
                (json!({ "people": n }), "rebuilt address suggestions".into())
            }
            "v1.drafts.list" => {
                let list: Vec<Value> = self.drafts()?.into_iter().map(|(id, saved, draft)| json!({ "id": id, "saved": saved, "draft": serde_json::from_str::<Value>(&draft).unwrap_or_default() })).collect();
                let n = list.len();
                (Value::Array(list), format!("{n} drafts"))
            }
            "v1.drafts.save" => {
                let draft: crate::compose::Draft = p.de("draft")?;
                let id = self.save_draft(p.opt_i64("id"), &serde_json::to_string(&draft).map_err(Error::from)?)?;
                (json!({ "id": id }), format!("saved draft {id}"))
            }
            "v1.drafts.delete" => {
                let id = p.i64("id")?;
                self.delete_draft(id)?;
                (json!({}), format!("deleted draft {id}"))
            }
            "v1.outbox.list" => (to(self.outbox()?)?, "the Out mailbox".into()),
            "v1.outbox.unqueue" => {
                let id = p.i64("id")?;
                let draft = self.unqueue(id)?;
                let draft_id = self.save_draft(None, &serde_json::to_string(&draft).map_err(Error::from)?)?;
                (json!({ "draft": draft_id }), format!("took {id} out of Out"))
            }
            "v1.outbox.reschedule" => {
                let (id, at) = (p.i64("id")?, p.i64("send_at")?);
                self.reschedule(id, at)?;
                (json!({}), format!("{id} goes at {at}"))
            }
            "v1.outbox.send_now" => {
                let sent = crate::outbox::send_due(self, true)?;
                (json!({ "sent": sent.sent, "failed": sent.failed, "notes": sent.notes }), format!("sent {} queued messages", sent.sent))
            }

            // ── the spam filter ──
            "v1.spam.state" => {
                let mut learned = Vec::new();
                for mbx in self.spam_filter_mailboxes()?.into_iter().filter(|x| x.on) {
                    for a in &mbx.accounts {
                        let (spam, ham) = self.bayes_counts(*a)?;
                        learned.push(json!({ "mailbox": mbx.shown, "spam": spam, "good": ham }));
                    }
                }
                (json!({ "on_arrival": self.spam_filter_on_arrival()?, "spamhaus": crate::blocklists::key().is_some(), "learned": learned }), "spam filter state".into())
            }
            "v1.spam.preview" => {
                let items: Vec<(i64, i64)> = p.de("items")?;
                for (id, folder) in &items {
                    scope.require(self, *id)?;
                    folders_ok(&[*folder])?;
                }
                (to(self.triage_messages(&items, &crate::Silent)?)?, format!("spam filter preview of {} messages", items.len()))
            }
            "v1.spam.apply" => {
                let chosen: Vec<crate::triage_decide::Chosen> = p.de("chosen")?;
                let ids: Vec<i64> = serde_json::to_value(&chosen).ok().and_then(|v| v.as_array().map(|a| a.iter().filter_map(|c| c["id"].as_i64()).collect())).unwrap_or_default();
                messages(&ids)?;
                let n = self.triage_apply(&chosen)?;
                (json!({ "moved": n }), format!("spam filter moved {n} messages"))
            }
            "v1.spam.mark" => {
                let items: Vec<(i64, i64)> = p.de("items")?;
                for (id, folder) in &items {
                    scope.require(self, *id)?;
                    folders_ok(&[*folder])?;
                }
                let spam = p.bool("spam", true);
                let (notes, moved) = self.mark_and_move(&items, spam)?;
                (json!({ "notes": notes, "moved": moved }), format!("{} {} messages", if spam { "Spam:" } else { "Not Spam:" }, items.len()))
            }
            "v1.spam.why" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                (to(self.triage_one(id)?)?, format!("why for message {id}"))
            }
            "v1.spam.rules" => (to(self.spam_filter_rules()?)?, "spam filter rules".into()),
            "v1.spam.rule_set" => {
                let key = p.str("key")?;
                let settings: crate::triage_decide::RuleSettings = p.de("settings")?;
                self.set_rule_settings(&key, &settings)?;
                (json!({}), format!("spam rule {key} changed"))
            }
            "v1.spam.entry_add" => {
                let (pattern, kind) = (p.str("pattern")?, p.str("kind")?);
                if kind != "block" && kind != "allow" {
                    return Err(bad("kind: block or allow"));
                }
                let action: Option<crate::triage::Action> = if p.0["action"].is_null() { None } else { Some(p.de("action")?) };
                let id = self.set_sender_rule_in(&pattern, &kind, action.as_ref(), &p.opt_str("note").unwrap_or_default(), &p.opt_str("mailbox").unwrap_or_default())?;
                (json!({ "id": id }), format!("{kind} {pattern}"))
            }
            "v1.spam.rule_remove" => {
                let key = p.str("key")?;
                if let Some(id) = key.strip_prefix("like:").and_then(|n| n.parse().ok()) {
                    self.remove_like_rule(id)?;
                } else {
                    let (kind, pattern) = key.split_once(':').ok_or_else(|| bad(format!("{key} can only be switched off")))?;
                    let entries: Vec<_> = self.sender_rules()?.into_iter().filter(|e| e.kind == kind && e.pattern == pattern).collect();
                    if entries.is_empty() {
                        return Err(bad(format!("{key} can only be switched off")));
                    }
                    for e in entries {
                        self.remove_sender_rule(e.id)?;
                    }
                    self.set_rule_settings(&key, &Default::default())?;
                }
                (json!({}), format!("removed {key}"))
            }
            "v1.spam.like_preview" => {
                let id = p.i64("id")?;
                messages(&[id])?;
                let (keys, count, sample) = self.messages_like(id, 365, 20)?;
                let what = keys.describe();
                (json!({ "keys": keys, "what": what, "count": count, "sample": sample }), format!("\"like\" rule from message {id}"))
            }
            "v1.spam.like_add" => {
                let name = p.str("name")?;
                let id = self.add_like_rule_in(&name, p.de("keys")?, p.de("action")?, &p.opt_str("mailbox").unwrap_or_default())?;
                (json!({ "id": id }), format!("\"like\" rule {name}"))
            }
            "v1.spam.mailboxes" => (to(self.spam_filter_mailboxes()?)?, "spam filter mailboxes".into()),
            "v1.spam.languages" => {
                let all: Vec<Value> = crate::triage_decide::LANGUAGES.iter().map(|(c, n, _)| json!({ "code": c, "name": n })).collect();
                (json!({ "chosen": self.primary_languages()?, "all": all }), "the user's languages".into())
            }
            "v1.spam.languages_set" => {
                let codes = p.strings("codes");
                if codes.is_empty() {
                    return Err(bad("codes: at least one language"));
                }
                self.set_primary_languages(&codes)?;
                (json!({}), format!("languages {}", codes.join(", ")))
            }
            "v1.spam.mailboxes_set" => {
                let list = p.strings("addresses");
                self.set_spam_filter_mailboxes(&list)?;
                (json!({}), format!("spam filter in {}", list.join(", ")))
            }
            "v1.spam.arrival" => {
                let on = p.bool("on", true);
                self.set_spam_filter_on_arrival(on)?;
                (json!({}), format!("sorting arriving mail {}", if on { "on" } else { "off" }))
            }
            "v1.spam.learn" => {
                let mut said = Vec::new();
                for mbx in self.spam_filter_mailboxes()?.into_iter().filter(|x| x.on) {
                    for a in &mbx.accounts {
                        let t = self.bayes_bootstrap(*a, &crate::Silent)?;
                        said.push(json!({ "mailbox": mbx.shown, "trained": t }));
                    }
                }
                (Value::Array(said), "learned again".into())
            }
            "v1.spam.spamhaus_key" => {
                crate::blocklists::set_key(&p.str("key")?)?;
                (json!({}), "Spamhaus key changed".into())
            }

            // ── preferences ──
            "v1.search.scope" => (to(self.search_scope_state()?)?, "search's mailboxes".into()),
            "v1.search.scope_set" => {
                let key = p.str("key")?;
                let searched = p.0.get("searched").and_then(|v| v.as_bool());
                (to(self.set_search_scope(&key, searched)?)?, format!("search switch {key}: {searched:?}"))
            }
            "v1.settings.get" => {
                let key = p.str("key")?;
                (json!({ "value": self.setting(&key)? }), format!("setting {key}"))
            }
            "v1.settings.prefix" => {
                let prefix = p.str("prefix")?;
                (to(self.settings_with_prefix(&prefix)?)?, format!("settings {prefix}*"))
            }
            "v1.signatures.get" => (to(self.signatures()?)?, "signatures".into()),
            "v1.views.accounts" => (to(self.views_accounts()?)?, "views' mailboxes".into()),
            "v1.settings.set" => {
                let (key, value) = (p.str("key")?, p.str("value")?);
                // the API's own settings are the user's alone
                if key.starts_with("api") {
                    return Err(denied("the API's own settings are changed only in Eudora's window"));
                }
                self.set_setting(&key, &value)?;
                (json!({}), format!("setting {key} changed"))
            }
            "v1.signatures.set" => {
                self.set_signatures(&p.de("signatures")?)?;
                (json!({}), "signatures changed".into())
            }
            "v1.labels.set_list" => {
                let list: Vec<crate::marks::LabelDef> = p.de("list")?;
                let renamed: Vec<(String, String)> = if p.0["renamed"].is_null() { Vec::new() } else { p.de("renamed")? };
                self.set_label_list(&list, &renamed)?;
                (json!({}), "labels changed".into())
            }
            "v1.views.accounts_set" => {
                let list = p.strings("addresses");
                self.set_views_accounts(&list)?;
                (json!({}), "views' mailboxes changed".into())
            }
            _ => return Err(Refusal { code: -32601, message: format!("there's no method {method}") }),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::api::{Grant, Limits, NoHost, Permission};
    use crate::library::{Access, Library};
    use crate::progress::Silent;
    use crate::{ImportOptions, import};

    /// A library with Inbox and Bank (a message each), and a program granted `grants`.
    fn setup(grants: Vec<Grant>) -> (tempfile::TempDir, Library, Limits, String, [i64; 2], [i64; 2]) {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        for (folder, n) in [("Inbox", 1), ("Bank", 2)] {
            let file = dir.path().join(format!("{n}.eml"));
            std::fs::write(&file, format!("From: ann@a.com\r\nTo: me@x.com\r\nSubject: Hello {n}\r\nMessage-ID: <{n}@a>\r\n\r\n<p>Hi <img src=\"https://tracker.example/{n}.png\"></p>\r\n")).unwrap();
            import(&mut library, &[file], &ImportOptions { destination: Some(folder.into()), ..Default::default() }, &Silent).unwrap();
        }
        let folder = |p: &str| library.folders().unwrap().into_iter().find(|f| f.path.ends_with(p)).unwrap().id;
        let (inbox, bank) = (folder("Inbox"), folder("Bank"));
        let ids: Vec<i64> = library.conn.prepare("SELECT id FROM messages ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        let limits = Limits::default();
        let asked = json!({ "name": "Tool", "publisher": "Test", "reason": "Testing", "secret": "s", "permissions": [{ "permission": "mail.headers.read" }] });
        let request = library.api_call(&limits, &NoHost, None, "v1.auth.request", &asked).unwrap()["request"].as_i64().unwrap();
        library.api_answer(request, Some(&grants)).unwrap();
        let token = library.api_call(&limits, &NoHost, None, "v1.auth.collect", &json!({ "request": request, "secret": "s" })).unwrap()["token"].as_str().unwrap().to_string();
        (dir, library, limits, token, [inbox, bank], [ids[0], ids[1]])
    }

    fn grant(p: Permission, folders: &[&str]) -> Grant {
        Grant { permission: p, accounts: vec![], folders: folders.iter().map(|f| f.to_string()).collect() }
    }

    #[test]
    fn the_rest_of_the_window_is_reachable_but_only_as_granted() {
        let inbox_path = "Inbox";
        let (_dir, library, limits, token, [inbox, bank], [in_inbox, _]) = setup(vec![
            grant(Permission::HeadersRead, &[inbox_path]),
            grant(Permission::StatusRead, &[]),
            grant(Permission::Compose, &[]),
            grant(Permission::SettingsWrite, &[]),
        ]);
        let t = Some(token.as_str());
        let call = |method: &str, params: serde_json::Value| library.api_call(&limits, &NoHost, t, method, &params);
        // a folder it may see; one it may not; All Mail (the whole library) it may not
        assert_eq!(call("v1.mail.count", json!({ "folders": [inbox] })).unwrap()["count"], 1);
        assert_eq!(call("v1.mail.count", json!({ "folders": [bank] })).unwrap_err().code, -32001);
        assert_eq!(call("v1.mail.page", json!({})).unwrap_err().code, -32001, "All Mail spans folders it can't see");
        assert_eq!(call("v1.mail.page", json!({ "folders": [inbox] })).unwrap().as_array().unwrap().len(), 1);
        // status, unscoped
        assert!(call("v1.library.info", json!({})).unwrap()["path"].is_string());
        // a reply holds the original's text: needs reading text too
        assert_eq!(call("v1.compose.start", json!({ "mode": "reply", "id": in_inbox })).unwrap_err().code, -32001);
        assert!(call("v1.compose.start", json!({ "mode": "new" })).is_ok());
        // the API's own settings are the user's alone, however much it's allowed
        assert_eq!(call("v1.settings.set", json!({ "key": "api-settings", "value": "{\"without_window\":true}" })).unwrap_err().code, -32001);
        assert!(call("v1.settings.set", json!({ "key": "completion", "value": "{}" })).is_ok());
        // never granted: running the library
        assert_eq!(call("v1.library.optimize", json!({})).unwrap_err().code, -32001);
        assert_eq!(call("v1.trash.empty", json!({ "accounts": [1] })).unwrap_err().code, -32001);
    }

    #[test]
    fn the_command_line_reaches_everything_and_is_logged() {
        let (_dir, library, _limits, _token, [inbox, _], [in_inbox, _]) = setup(vec![grant(Permission::HeadersRead, &[])]);
        let r = library.api_owner_call(&NoHost, "v1.mail.view", &json!({ "id": in_inbox })).unwrap();
        assert!(r["text"].as_str().unwrap_or_default().contains("Hi") || r["html"].as_str().unwrap_or_default().contains("Hi"), "{r}");
        assert_eq!(library.api_owner_call(&NoHost, "v1.mail.count", &json!({})).unwrap()["count"], 2);
        assert!(library.api_owner_call(&NoHost, "v1.labels.list", &json!({})).unwrap().as_array().is_some());
        library.api_owner_call(&NoHost, "v1.mail.mark", &json!({ "ids": [in_inbox], "status": "unread" })).unwrap();
        assert_ne!(library.flags_of(in_inbox).unwrap() & crate::marks::UNREAD, 0);
        let moved = library.api_owner_call(&NoHost, "v1.mail.copy", &json!({ "ids": [in_inbox], "to": inbox })).unwrap();
        assert_eq!(moved["copied"], 0, "already there");
        assert!(library.api_log(None, 50).unwrap().iter().any(|l| l.detail.starts_with("command line: ") && l.method == "v1.mail.mark"));
        // the API's own management isn't a method at all
        assert_eq!(library.api_owner_call(&NoHost, "v1.api.answer", &json!({})).unwrap_err().code, -32601);
    }
}
