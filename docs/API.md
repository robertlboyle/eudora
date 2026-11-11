# Phase 2 proposal: a local API and plugins, securely

Status: **decided 2026-10-16**: WebAssembly plugins; sending confirmed per message with "allow all from this plugin" once trusted; unsigned plugins after a warning; API off while the window is closed unless the user overrides; a full log of every API read and write kept in the library, on by default, can be turned down per client once trusted.

The standing rule: every program is scriptable, has an API and takes plugins, **securely**. The case to guard against: a bad program must not be able to quietly search someone's mail for bank details and send them somewhere.

## What exists already
- **The `eudora-cli` command line** covers most of the window: accounts, import, search, list, show, rules, triage / spam, mark, labels, empty-trash, backup / restore, optimize, offline, signatures, spell, mail-stats, views, graph download… It opens the library directly, so it can do anything: it's for the owner of the computer, not for other programs.
- **`eudora-web`**: a mail API over HTTPS for the user's own phone (password + passkeys). It's for people, not programs.

## What gets built

### 1. One core of commands (the "service layer")
Every action the window, the command line, the web interface and the API can take goes through **one table of commands**: name, arguments, result, and **the permission it needs**. The window's Tauri commands, the CLI's subcommands and the API's methods all call into it, so they can't drift apart, and the API automatically covers every function the window has.

### 2. The local API
- **Transport:** JSON-RPC 2.0 over a **named pipe** on Windows (`\\.\pipe\\eudora-<user>`) and a **Unix socket** on the Mac (`~/Library/Application Support/Eudora/api.sock`, mode 0600). **No network port at all**: other computers can't reach it, and neither can web pages.
  - On Windows the pipe's security descriptor allows only the signed-in user.
- **Versioned:** methods are named `v1.mail.search`, `v1.mail.send`… Version 1 stays as it is once released; changes come as `v2.…`.
- **Served by:** the window while it's open; by `eudora-cli api serve` when it isn't (a command line service; it can run at startup).
- **Documented:** a reference of every method, its arguments, results and permission, generated from the command table. It becomes a chapter of the manual.

### 3. Who may do what: clients and permissions
- **Nothing is reachable until the user grants it.** A program asks once (`v1.auth.request`: its name, its publisher, why it wants it, the permissions it asks for). Eudora shows the user a window, **"Allow 'Invoice Exporter' to: read headers in Inbox…?"** Each permission is a tick box, so the user can grant less than asked. The program gets **its own token**.
  - The token is kept by Windows' DPAPI / the Mac Keychain on the client's side; Eudora keeps only its hash.
- **Permissions are narrow and named:**

  | Permission | What it allows |
  |---|---|
  | `accounts.read` | addresses, names and sending addresses; never secrets |
  | `mail.headers.read` | sender, subject, date, folders, labels, status; lists, counts, Views, the Trash |
  | `mail.bodies.read` | the text of messages, their source, their pictures from the web |
  | `mail.attachments.read` | attachment bytes |
  | `mail.search` | search; results limited to what the other grants allow reading |
  | `mail.change` | file, copy, label, mark read, priority; run the rules |
  | `mail.delete` | delete, Empty Trash |
  | `mail.send` | always shown to the user before it goes, unless they allowed all from that program; the Out mailbox |
  | `mail.compose` | drafts, replies and forwards (which also need `mail.bodies.read`), spelling, address suggestions |
  | `rules.read` / `rules.write` | |
  | `spam.filter` | sorting, Spam / Not Spam, the spam filter's rules and lists, learning, the Spamhaus key (write-only) |
  | `settings.read` / `settings.write` | preferences, labels, signatures, Views' mailboxes (never the API's own settings) |
  | `accounts.manage` | add and change accounts and servers, shared mailboxes, sign in, forget sign-ins |
  | `status.read` | downloads, changes waiting, sync, statistics, plugins, the firewall |
  | `library.manage` | check mail, pause, offline, the background download, back up (never with passwords), restore, import, optimize, old attachments, sync, plugins, the firewall |
  | `contacts.read` / `contacts.write` | the address book: list, read, export / add, change, merge, delete (on the servers too) |

  Each one can be limited to **some accounts and folders** ("Inbox of pat@example.com only"); whatever works on the whole library (All Mail, statistics, a backup) needs an unlimited grant.
- **Every function of the window is a method**: 113 in version 1, `eudora-cli api methods` lists them; the command line runs any of them as the owner with `eudora-cli call METHOD JSON`. A test keeps every window command either in the API or on the short window-only list.
- **What no program can do, by design:** grant itself permissions, approve its own sends, raise its own trust, change the API's settings (the user's alone, in the window); read secrets (passwords, sign-in tokens, the sync recovery phrase, the Spamhaus key; a backup through the API never holds passwords).
- **Sensitive data behind the strongest grants:** bodies and attachments are separate from headers. The grant window says it plainly: *"can read the full text of your mail"*.
- **One place to see and revoke:** Preferences ▸ **Programs and plugins** lists every client and plugin with its permissions, when it last did something, and Revoke.
- **Audit log:** every call (client, method, what it touched as counts and folders, never the mail itself, allowed or refused) is kept in the library itself (`api_log`), shown in that window (and `eudora-cli api log`), kept 90 days. Full by default; per program it can be turned down to changes only, or refusals only, once trusted.
- **Rate limits** per client: for example, bodies at most 1,000 messages a minute by default. A burst beyond that is refused and logged, so a "read everything and ship it out" program is slowed and visible.
- **An extra line of defense against the bank-details case:** a client granted `mail.bodies.read` that also asks to send, or to reach the network through Eudora, gets a **separate, explicit warning**. Eudora can't stop a program from using the network itself; it can make sure the user sees what they're granting, and log it.

### 4. Plugins
- **What a plugin can add:** menu items (in a "Plugins" menu, and in the right-click menu), commands, a side panel or dialog view (HTML, shown in a sandboxed frame like mail), and rule actions ("Run plugin X" in a rule).
- **How it runs:** isolated, never inside Eudora with its powers. ❓ Two ways (see decision 1):
  - **(A) A separate process.** The plugin is a program Eudora starts; it talks to Eudora only through the API, with the permissions the user granted it. Any language. Simple, no new library. The isolation is the operating system's process boundary plus the API's permissions. It can still use the network and files itself, like any program the user runs.
  - **(B) WebAssembly.** The plugin is a .wasm file run inside Eudora by a WebAssembly engine (wasmtime, Apache-2.0 / MIT). It can do **nothing** but what Eudora hands it: no network, no files, no other programs, unless granted. This is the strongest isolation, the "bad plugin can't send the bank details anywhere" guarantee. The cost: a large new library (wasmtime, about 10-15 MB in the program, and a longer build), and plugins must be written for WebAssembly (Rust, C, Go, AssemblyScript…).
- **Identity:** each plugin has a manifest (name, publisher, version, permissions asked, menu items), shown at install time.
  - ❓ **Signing** (decision 3): unsigned plugins allowed after a warning, or signed only.
- **Installing:** Tools ▸ Plugins ▸ Install… (a .eudora-plugin file = manifest + program or .wasm), then the same grant window as API clients.

### 5. The command line
- **Stays the owner's tool** (full access, as now).
- **Gains `eudora-cli api …` to manage the API:** `serve`, `clients`, `revoke`, `audit`.

## Decisions for the user (❓)
1. **Plugin isolation:**
   - **(A) separate processes** (no new library, any language, the OS keeps them apart; a plugin can still use the network on its own), or
   - **(B) WebAssembly** (wasmtime: a new, large, permissive library; plugins can do nothing they weren't granted, not even reach the network).
   - Or **both**: A for trusted tools, B for plugins from strangers.
   - **Recommended: B for plugins (the user's security worry is exactly what it prevents), A only for "programs" using the API.**
2. **Sending through the API:** always confirmed by the user in a window (recommended), or allowed without asking for programs the user marks as trusted.
3. **Unsigned plugins:** allowed after a clear warning (recommended for now; there's no signing service yet), or signed only.
4. **Where the API runs when the window is closed:** `eudora-cli api serve` started by hand or at sign-in (recommended: off until the user turns it on in Preferences), or never (the API only while the window is open).

## Build order once decided
1. The command table and permission model in the core, with tests (permissions, scopes, rate limits, audit).
2. The pipe / socket server (JSON-RPC), client tokens, the grant window, Preferences ▸ Programs and plugins, the audit view.
3. The API reference, generated from the table; example clients (PowerShell, Python).
4. Plugins: manifest, install, menu items / views / rule actions, the isolation chosen.
5. A test plugin and a hostile one (tries to read bodies and reach the network without grants), checked to fail.
