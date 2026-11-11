# Classic Eudora's UI, and what Eudora does with it

An inventory of Eudora 7.1 for Windows (`Eudora_W_source\Eudora71\Eudora\EudoraRes.rc` and
`resource.h`, from the Computer History Museum's BSD-licensed release), with a decision for
each part:

- **Keep**: the same command or setting, under the same name and shortcut;
- **Modernize**: the same idea, done the way it makes sense today;
- **Drop**: no longer meaningful (dial-up, MDI windows, Winsock tuning…). POP3 stays.

**accepted, with two modes** (below). On
macOS, Ctrl shortcuts become ⌘.

## Two modes: Classic and Modern

The look and the keystrokes switch between **Classic**, identical to classic Eudora, and
**Modern**, a more modern take on it updated for today's users and standards.

- **Classic mode**: menus, names, shortcuts and icons identical to classic Eudora 7.1. Icons are
  **redrawn versions of classic Eudora's own icons** (from `Eudora_W_source`, as the program
  icon was), including the status column's classic marks. **Every new Eudora feature is in
  Classic mode too**, placed where classic Eudora would have put it, with
  Classic-mode icons.
- **Dropped features are removed entirely in both modes**: no grayed-out
  menu items. **The help explains each one**: a help search for a removed feature (dial-up,
  Kerberos, plug-ins, MoodWatch…) finds why it was removed or deprecated and what replaces it,
  if anything. The help's "Removed features" page is built from this file's Drop rows.
- **Modern mode**: the Keep / Modernize decisions in this file, Font Awesome icons, and
  Eudora's own shortcuts where they differ (e.g. Ctrl+Shift+F = Sort with the Spam Filter;
  in Classic mode Ctrl+Shift+F stays Find Text).
- Independent of the look (Eudora, Thunderbird-style, Lookout, Gmail-style, Yahoo-style): the
  mode decides Eudora's own menus, keys and icons.
- **Ctrl+R = Reply** in both modes (Ctrl+Shift+R = Reply to All; classic's "Map Ctrl+R to Reply
  to All" option stays).

The Thunderbird-style skin uses the same commands, with Thunderbird's menu layout. That
mapping comes later and doesn't depend on this file.

## Main menu bar

Classic order: **File · Edit · Mailbox · Message · Transfer · Special · Tools · Window · Help**.
Eudora keeps it.
- **Mailbox** (open a mailbox) and **Transfer** (move the selected messages to a mailbox) are
  built from your folders at run time, as before, with a type-to-find field at the top for
  thousands of folders.

### File
| Classic | Shortcut | Decision |
|---|---|---|
| New File | | Drop (Eudora edited plain text files) |
| Open File… | Ctrl+O | Modernize: open an .eml / .mbox / .mbx file (viewed, or imported) |
| Open Attachment | Enter | Keep |
| Close | Ctrl+W | Keep (closes the tab or window) |
| Save · Save As… | Ctrl+S | Keep (Save = save draft; Save As = .eml, .txt, .pdf) |
| Save As Stationery | | Keep as **Save as Template** (Stationery is kept as the name in the Eudora skin) |
| Send Queued Messages | Ctrl+T | Keep (the Out mailbox: queued and scheduled sends) |
| Check Mail | Ctrl+M | Keep (syncs every account now) |
| Work Offline | | Keep |
| Print Setup… · Print Preview · Print… · Print One | Ctrl+P | Modernize: Print… (with preview, the system dialog); Print One = print without the dialog |
| Import… | | Keep: the importers (Phase 5) |
| Exit | Ctrl+Q | Keep |

### Edit
| Classic | Shortcut | Decision |
|---|---|---|
| Undo · Redo | Ctrl+Z · Ctrl+Shift+Z | Keep (also for moves and deletes of messages) |
| Cut · Copy · Paste · Clear | Ctrl+X/C/V | Keep |
| Paste Special… | | Keep as Paste as Plain Text |
| Paste As Quotation | Ctrl+' | Keep |
| Text ▸ Plain/Bold/Italic/Underline/Strikeout/Typewriter | Ctrl+B/I/U | Keep (Typewriter = monospace) |
| Text ▸ Add / Remove Quote Level | Ctrl+Shift+. / Ctrl+. | Keep |
| Text ▸ Size (Very Small … Humongous) | | Keep, Humongous included |
| Text ▸ Color · Last Text Color · Font · Margins · Left/Right/Center · Bulleted List · Make Hyperlink · Format Painter · Clear Formatting (Ctrl+Space) | | Keep |
| Insert ▸ Picture · Downloadable Picture · Horizontal Line | | Keep (Downloadable Picture = an image by link) |
| Insert Emoticon | | Modernize: emoji picker |
| Select All | Ctrl+A | Keep |
| Wrap Selection | | Keep (plain text) |
| Finish Address Book Entry | Ctrl+, | Keep (complete the name being typed) |
| Insert Recipient ▸ | | Keep (your quick-recipient list) |
| Find ▸ Find Text · Find Text Again | Ctrl+Shift+F · F3 / Ctrl+G | Keep (in the open message) |
| Find ▸ Find Messages | Ctrl+F | Keep: **the boolean search over everything**, the heart of Eudora |
| Find ▸ Search Web / Eudora / Mailbox / Mailfolder (for the selection) | | Keep |
| Sort ▸ Status, Priority, Attachment, Label, Sender, Date, Size, Subject · Group by Subject | | Keep. Group by Subject becomes **Group by Conversation** (real threads) |
| Sort ▸ Server Status | | Keep for POP accounts (left on server / to delete) |
| Sort ▸ Mood · Junk | | Drop Mood (see MoodWatch); Junk kept |
| Check Spelling | Ctrl+6 | Keep |
| Message Plug-ins ▸ | | Drop (the old EMSAPI plug-in system; an extension API may come much later) |

### Message
| Classic | Shortcut | Decision |
|---|---|---|
| New Message | Ctrl+N | Keep |
| Reply · Reply to All | Ctrl+R | Keep (option: Ctrl+R = Reply to All, as classic) |
| Forward | | Keep |
| Redirect | | Keep where the account can (IMAP/SMTP: Resent- headers); Microsoft 365 and Gmail can't truly redirect, so there it forwards "as is" and says so |
| Send Again | | Keep (a copy of a sent message as a new draft) |
| New Message To ▸ · Forward To ▸ · Redirect To ▸ | | Keep (quick-recipient list) |
| New Message With ▸ · Reply With ▸ · Reply to All With ▸ | | Keep (Stationery = templates) |
| Attach File | Ctrl+H | Keep |
| Attach ▸ | | Keep (recent attachments / files) |
| Send Immediately | Ctrl+E | Keep |
| Change ▸ Queuing… | | Modernize: **Send Later…** (date and time) |
| Change ▸ Status ▸ Unread / Read / Replied / Forwarded / Redirected / Recovered | | Keep Unread / Read / Replied / Forwarded; Redirected kept where supported; Recovered dropped |
| Change ▸ Status ▸ Unsendable / Sendable / Queued / Time Queued / Sent / Unsent | | Modernize: Draft / Queued / Scheduled / Sent |
| Change ▸ Priority ▸ Highest … Lowest | | Keep (the 5 levels map to the Importance / X-Priority headers) |
| Change ▸ Label ▸ None, Label1-7 | | Modernize: **Labels = categories**: any number, named and colored, synced as Outlook categories / Gmail labels / IMAP keywords |
| Change ▸ Server Status ▸ Leave / Fetch / Delete / Fetch then Delete | | Keep for POP accounts |
| Change ▸ Personality ▸ | | Keep: Personalities = your identities (accounts and their From addresses) |
| Junk · Not Junk | Ctrl+Shift+J · Ctrl+Alt+J (Classic mode: Ctrl+J, as the classic option) | Keep (to / from Spam; teaches Eudora's spam filter: blacklist / whitelist + "like this" rules) |
| Delete · UnDelete · Purge Messages | Ctrl+D | Keep Delete / UnDelete (Trash); Purge = Empty Trash for a folder |

### Special
| Classic | Shortcut | Decision |
|---|---|---|
| Filter Messages | Ctrl+J | Keep: run the rules on the selection |
| Make Filter | | Keep: a rule from the selected message (From, any recipient, Subject; transfer / delete) |
| Make Address Book Entry… | Ctrl+K | Keep |
| Add as Recipient · Remove Recipient ▸ | | Keep (the quick-recipient list) |
| Empty Trash | | Keep |
| Compact Mailboxes | | Modernize: **Optimize Library** (compact packs, merge index segments, verify) |
| Forget Password(s) · Change Password… | | Modernize: Forget Sign-ins (tokens in the OS secret store); passwords are changed with the provider |
| Message Plug-ins Settings… | | Drop |

### Tools
| Classic | Shortcut | Decision |
|---|---|---|
| Filters · Filter Report | | Keep: the rules window, and a report of what the rules did |
| Mailboxes | | Keep: the folder tree, all accounts |
| File Browser | | Drop |
| Stationery · Signatures · Personalities | | Keep |
| Task Status · Task Errors | | Modernize: **Activity**: sync, imports, indexing, sending, with real counts ("Downloading headers: 48,000 of 3,100,000") and errors |
| Address Book | Ctrl+L | Keep (contacts from every account, plus local) |
| Directory Services | Ctrl+Y | Modernize: the organization's directory (Microsoft 365 / Google Workspace people search; LDAP later); Ph dropped |
| Link History | | Drop |
| Statistics | | Modernize later: mail statistics (sent / received over time), easy on our index |
| Reset Tools… | | Keep as Reset Window Layout |
| Options… | | Keep: Tools ▸ Options (see below); on macOS also Settings… in the app menu |

### Window
| Classic | Shortcut | Decision |
|---|---|---|
| Cascade · Tile Horizontal · Tile Vertical · Arrange Icons · Send To Back | Shift+F5 · Shift+F4 · Ctrl+F6 | Modernize: no MDI. Tabs and separate windows, with a list of open windows (Ctrl+F6 = next) |

### Help
| Classic | Decision |
|---|---|
| Topics · Context Sensitive Help (F1 · Shift+F1) | Modernize: online help; F1 opens the page for where you are |
| Technical Support · Report A Bug | Keep (links: issue tracker) |
| Tip of the Day | Drop |
| Payment & Registration | Drop (free) |
| Insert System Configuration | Modernize: Copy Diagnostic Information |
| About Eudora… | Keep |

## Keyboard shortcuts to keep (muscle memory)

From the `IDR_MAINFRAME` accelerator table:

| Keys | Command |
|---|---|
| Ctrl+N | New Message |
| Ctrl+R (Shift: the other one) | Reply (or Reply to All, an option) |
| Ctrl+E | Send Immediately |
| Ctrl+T | Send Queued Messages |
| Ctrl+M | Check Mail |
| Ctrl+D | Delete |
| Ctrl+H | Attach File |
| Ctrl+J | Filter Messages (or Junk, an option, as classic); Modern mode: Junk = Ctrl+Shift+J, Not Junk = Ctrl+Alt+J |
| Ctrl+K | Make Address Book Entry |
| Ctrl+L | Address Book |
| Ctrl+Y | Directory Services |
| Ctrl+F · Ctrl+Shift+F | Find Messages · Find Text |
| F3 / Ctrl+G | Find Again |
| Ctrl+1 · Ctrl+0 | Open the In mailbox · the Out mailbox |
| Ctrl+2 … Ctrl+9 | Open the mailboxes you assign (classic: plug-in / custom) |
| Ctrl+6 | Check Spelling |
| Ctrl+' | Paste As Quotation |
| Ctrl+, | Finish Address Book Entry |
| Ctrl+. · Ctrl+Shift+. | Remove · Add Quote Level |
| Ctrl+\ | Process All (classic `ID_PROCESS_ALL`: send, check and run filters) |
| Ctrl+Shift+S | Save All |
| Ctrl+Shift+W | Close All |
| F6 · Shift+F6 | Next · previous pane |
| **F7** | **Show all headers** (classic "Blah Blah Blah" toggle: kept, with that name as its tooltip) |
| Ctrl+W · Ctrl+Q · Ctrl+P · Ctrl+S · Ctrl+O | Close · Exit · Print · Save · Open |
| Arrow keys / Ctrl+arrows / Alt+arrows | Next / previous message (classic option "Switch messages with") |

## Context menus

- **Message list** (`Mailbox`, `ImapMailbox`): Reply · Reply to All | Make Filter | Change Status / Priority / Label | Junk · Not Junk | Delete · UnDelete. *Keep*, plus Move to ▸ and Copy to ▸.
  - Change Server Status: kept for POP accounts.
  - Recalculate Junk Score kept as **Sort with the Spam Filter** / **Why? (Spam Filter)**. Dropped: IMAP Fetch ▸ / Remove Cached Contents (our library decides what's downloaded).
- **Open message** (`Received Msgs`): Copy | Search Web / Eudora / Mailbox / Mailfolder | Transfer to | Reply / Reply All / Forward / Redirect | Make Filter | View Source | Send to Browser | Open Attachment With… / Save Attachment As… / Explore Attachment (*Show in Folder*) / Copy Attachment… / Delete Attachment… | Junk / Not Junk | Delete. *Keep*.
- **Composing** (`Comp Message`): Cut / Copy / Paste / Paste Special / Paste As Quotation | Search … | Attach File | Send Again | Change Queuing… (*Send Later…*) | Check Spelling. *Keep*.
- **Out mailbox**: Send Immediately / Send Again | Make Filter | Change Queuing… | Change Status / Priority / Label | Delete. *Keep*, modernized as for the menus.
- **Address book** (`Nickname`): New / Delete / Rename | Move To / Copy To | Add to / Remove from Recipient List | Add to / Remove from BossWatch List. *Keep*.
- **System tray**: Exit | Minimize to Tray | Always Show Tray Icon | Work Offline | New Message | Check Mail | Open Eudora. *Keep* (macOS: the menu bar extra / Dock menu).

## Options (Tools ▸ Options), in classic order

| # | Category | Classic settings | Decision |
|---|---|---|---|
| 1 | Getting Started | Real name, email address, incoming server, user name, SMTP server, allow authentication | Modernize: **Add Account** (sign in with Microsoft / Google, or IMAP + SMTP with auto-discovery) |
| 2 | Checking Mail | Server, user name, check every N minutes, don't check without network / on battery, send on check, SSL info | Keep: sync interval per account (push where available), don't sync on battery / metered networks, send on check; SSL always on (no option) |
| 3 | Incoming Mail | POP/IMAP, leave mail on server, delete after N days, IMAP prefix, skip messages over N K, minimal headers / full except attachments over N K, when deleting: mark / move to, authentication style (passwords, Kerberos, APOP, RPA) | Keep per account: **POP** (leave mail on server, delete from server after N days, skip messages over N MB, passwords / APOP / OAuth where offered); IMAP: when deleting → move to Trash / mark deleted; download attachments over N MB only when opened (our attachment rule). Drop Kerberos, RPA, IMAP prefix (automatic) |
| 4 | Sending Mail | Allow authentication, immediate send, send on check, submission port 587, email address, default domain, SMTP server, relay personality, SSL | Modernize: per-account sending (TLS always; port automatic); keep **Immediate send** vs queue (the Out mailbox) and the default domain for bare names |
| 5 | Composing Mail | Default stationery, signature, word wrap, keep copies, quoted-printable, tabs in body, show signature while composing | Keep stationery, signature, word wrap, keep copies (save sent mail), show signature while composing; drop quoted-printable / tabs (automatic) |
| 6 | Internet Dialup | Dial-up networking entry, hang up, close on exit | **Drop** |
| 7 | Replying | Map Ctrl+R to Reply to All, copy priority, Fcc to original mailbox, signature on reply, quote only the selection, multiple messages on reply to multiple, reply-all: include yourself / original To: in Cc: | **Keep all** |
| 8 | Attachments | Encoding MIME/BinHex/Uuencode, attachment directory, text attachments in body, MIME digest as mailbox, delete attachments with Trash, delete automatic attachments | Keep: attachment folder (our `Attachments` with year folders), delete files with Trash; MIME only (BinHex/uuencode only read, never written); digests opened as a mailbox |
| 9 | Fonts | Proportional / fixed fonts and sizes for screen, message and printer | Keep: message font, fixed-width font, print font |
| 10 | Display | Toolbar, tips, cool bars, large buttons, status bar, category icons, MDI task bar, download HTML graphics, images inline, emoticons as pictures, minimize to tray, always show tray icon | Keep: toolbar (icons / labels / large), status bar, **don't download remote images** (privacy; per-sender allow), images inline, tray; drop cool bars / MDI task bar |
| 11 | Viewing Mail | Microsoft's viewer, separate IE settings, window size, preview pane, mark read after N seconds, open next message, zoom windows, allow executables in HTML | Keep: preview pane (right / below / off), mark as read after N seconds, open next after delete; **never** run scripts; drop IE options |
| 12 | Mailboxes | Columns (Status, Junk, Priority, Attachment, Label, Who, Date, Size, Server status, Mood, Subject), mailbox lines, count of selected in size box, close messages with mailbox, select newly inserted messages (FUMLUB), recent mailbox menu count, drag-and-drop transfers, drag-select | Keep all but Mood (Server status only in POP accounts); columns chosen per folder; FUMLUB keeps its classic acronym in the tooltip |
| 13 | Styled Text | Formatting toolbar; send both plain and styled / styled only / plain only / ask; styled signature | Keep |
| 14 | Spell Checking | Ignore capitalized / all caps / numbers / mixed case, doubled words, suggestions, as you type / on request, ignore quoted text, warn on send | Keep (a permissive spell library: Hunspell dictionaries) |
| 15 | Auto-completion | Sources (history, address book, plug-ins), list only, drop-down, alphabetical, add From: of replied-to, delay, history size | Keep (sources: history, contacts, directory) |
| 16 | Date Display | Local / sender's time zone; fixed or **age-sensitive** formats (RECENT / OLD / ANCIENT with hour limits) | **Keep, including age-sensitive dates**: a much-loved Eudora detail |
| 17 | Labels | 7 colors | Modernize: categories (any number, synced; see Message ▸ Change ▸ Label) |
| 18 | Getting Attention | Alert dialog, open mailbox, play a sound, filter report | Modernize: system notifications, sound, open mailbox, filter report |
| 19 | Background Tasks | Task status columns, wait for N seconds idle before filtering, bring error / status window to front | Mostly drop (we filter as mail arrives, without blocking); keep "show the Activity window on errors" |
| 20 | Automation | OLE automation, warn on auto-send | **Drop** (a scripting API may come much later) |
| 21 | Extra Warnings | Delete unread / queued / unsent mail, delete N+ messages, delete from search results, no subject, message over N K, quit with queued mail, empty Trash, not the default mailer, launch a program, forward files outside the attachment folder, invalid server | Keep the useful ones: unread / unsent / N+ messages, no subject, big message, quit with queued mail, empty Trash, not the default mail app. Drop the launch / server ones |
| 22 | MAPI | Eudora as the MAPI server (never / while running / always), MAPI attachments | Modernize: **be the default mail app** (mailto: and "Send to mail recipient" on Windows via Simple MAPI, macOS default mail app) |
| 23 | Advanced Network | Timeouts, buffer size, cache network info, unload Winsock | **Drop** |
| 24 | Kerberos | Realm, service, POP port | **Drop** |
| 25 | MoodWatch | Flag offensive mail ("on fire") and delay sending angry mail | **Comes back later** as a tone check before sending, with the built-in language model |
| 26 | Statistics | Graph colors, series, reset | Later (Tools ▸ Statistics) |
| 27 | Miscellaneous | Switch messages with arrows / Ctrl / Alt, empty Trash on exit, OK alerts after N s, turbo redirect, Return-Receipt-To, expand nicknames, auto-save every N minutes, hide address books from viruses | Keep: switch-message keys, empty Trash on exit, expand nicknames, auto-save drafts; drop the rest |
| 28 | Junk Mail | Junk score threshold, address book = not junk, Not-Junked senders into the address book, Junk mailbox, never unread, ignore date, trim junk older than N days, warn | Modernize: Eudora's own spam filter (the project's design notes; the server's verdict is one signal); keep "contacts are never junk" (the whitelist), Not Junk whitelists the sender, empty Spam after N days |
| 29 | Junk Mail Extras | Score of junked mail, trim threshold, Ctrl+J for Junk, delete fetched junk from server | Keep only "Ctrl+J for Junk" |
| 30 | Content Concentrator | Collapse quoted / repeated text in preview and message window, multiple messages | Modernize: **collapse quoted text** and conversation view |
| 31 | BossWatch | Warn when sending to more than N, on Reply All, outside / inside domain, color-code addresses | **Keep**: still very useful (external-recipient and reply-all warnings) |
| 32 | Find Messages | Fast indexed search, index now, re-index, index progress, include deleted IMAP | Keep: our Tantivy index (always on), Rebuild Index…, progress in Activity |

Eudora adds options classic didn't have: accounts (Microsoft 365, Gmail, IMAP), settings
sync, the skin (Eudora / Thunderbird-style), the library location, the page and index memory
limits, attachments download days, the appearance (light / dark), remote images, and the
importance flagging.

## Filters (Tools ▸ Filters): classic model → Eudora rules

Classic filter:
- **When to apply:** Incoming / Outgoing / Manual.
- **Up to two conditions**, joined by *ignore* / *and* / *or* / *unless*. Each one is a header (`From`, `To`, `Subject`, `<<Any Header>>`, `<<Any Recipient>>`, `<<Body>>`, `<<Personality>>`…) with a test: *contains*, *doesn't contain*, *is*, *is not*, *starts with*, *ends with*, *appears*, *doesn't appear*, *intersects nickname*, *matches regexp*.
- **Up to five actions:** Make Status, Make Priority (incl. raise / lower), Make Label, Make Personality, Make Subject, Play Sound, Speak, Open, Print, Notify User, Notify Application, Forward To, Redirect To, Reply With, Server Options, Copy To, Transfer To, Junk, Skip Rest.
- Filters run top to bottom; Skip Rest stops.

Eudora keeps that model and its names, and lifts the limits:
- any number of conditions, nested AND / OR / NOT;
- a condition can be **any boolean search** (the same language as Find Messages);
- any number of actions;
- rules can run on old mail too (as a search: "Applying rule: 18,000 of 42,000 messages").

Run-once protection across computers (stamps + ETags) is in the project's design notes. Make Filter stays one click from a message.

Actions:
- **Kept:** Make Status, Priority, Label (= category), Subject, Personality; Copy To / Transfer To; Junk; Forward / Redirect / Reply With; Notify (system notification), Play Sound, Open, Print; Skip Rest.
- **Dropped:** Notify Application. Server Options is kept for POP accounts.
- **Modernized:** Speak (via the OS voice).

## Concepts carried over

- **In / Out / Junk / Trash mailboxes**, the **Out mailbox as a send queue** (Send Queued Messages, Send Immediately, Send Later), and **Transfer** as the move command.
- **Personalities** (identities), **Stationery** (templates), **Signatures**.
- **Status column letters and marks**: • unread, R replied, F forwarded, D redirected, S sent, Q queued, `-` draft. Classic mode: redrawn classic icons; Modern mode: Font Awesome icons.
- **Age-sensitive dates**, **FUMLUB**, **Blah Blah Blah** (F7, all headers), **Humongous** text size: small details users remember.
- **Address book "nicknames"** (Ctrl+K, Ctrl+L, Finish Address Book Entry): now contacts synced from the accounts.

## Not carried over

- **Gone from today's mail:** RPA, Kerberos; dial-up; Winsock tuning. POP3 stays, for old mailboxes.
- **Gone from today's desktops:** MDI window arranging; the EMSAPI plug-ins; OLE automation; the File Browser; Link History.
- **No longer needed:** Payment & Registration; Tip of the Day; MoodWatch (until the language model stage); BinHex / uuencode for *sending* (still read); Microsoft's HTML viewer options; executables in HTML (never).

## Code worth porting (BSD; keep the copyright notice)

Candidates to read before writing our own; port only where it's clearly better than a
modern Rust library:
- **Mailbox `.toc` reading** (status, priority, labels, personality per message) for the `.mbx` importer (Phase 5). The format is only documented by this code.
- **`descmap.pce`** (the folder tree) and attachment-folder handling ("Attachment Converted:" lines) for the importer.
- **Filter evaluation semantics** (*intersects nickname*, *appears*, header aliases like `<<Any Recipient>>`), so imported classic filters behave the same.
- **Classic filters file** (`FILTERS.PCE`) and **nicknames file** (`NNdbase.txt` / `.txt` address books) readers, to import a user's filters and address book.
- **Age-sensitive date formatting** rules.
