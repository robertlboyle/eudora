# Eudora

A fast, free email program for Windows, the Mac, and Linux (with Android and iOS versions on the
way), built for huge mailboxes: tens of millions of messages and decades of mail stay quick to
open, scroll, and search.

- **Every account in one place:** Microsoft 365 and Outlook.com (with shared mailboxes and the
  Online Archive), Gmail and Google Workspace, IMAP, and POP3, in one library with one search.
  Imports classic Eudora mailboxes, mbox, and .eml files.
- **Instant search** with Outlook's search words and any header.
- **An inbox for people:** a spam filter that learns, fraud warnings, and marketing,
  notifications, and mailing lists sorted aside; rules; Views such as Receipts, Invoices, and Red
  Alert.
- **Calendar, contacts, and To Do**, from Microsoft and Google.
- **Looks** for every taste: Eudora Classic and Modern, and looks in the style of other popular
  mail programs, with dark mode.
- **Scriptable:** a command line for every function, a local API with permissions you grant, and
  plugins that run sandboxed in WebAssembly.
- **Private:** your mail stays on your computer and your mail servers. Eudora collects nothing
  about you, has no ads, and sends nothing anywhere you didn't ask it to.

Ready-made versions: [software.planet.net](https://software.planet.net/eudora.html).

## Official versions and builds from this source

To use Eudora day to day, install the official version from
[software.planet.net](https://software.planet.net/eudora.html): it's always the newest version,
and it keeps itself up to date, installing each new version as it comes out. Anything built from this
repository, including the packages GitHub Actions makes on each commit (a run's page ▸
Artifacts), is the same program but differs from an official version:

- **No Microsoft 365 or Outlook.com sign-in.** Microsoft accepts sign-ins only from a registered
  app, and the official app ID isn't in the published source. To use those accounts, register an
  app of your own in Microsoft Entra and build with its ID (see
  [BUILDING.md](BUILDING.md#a-microsoft-app-registration)). Microsoft then shows your app's name in
  its consent prompt, marked "unverified", and some organizations allow only apps their
  administrator has approved.
- **No Google sign-in** (Gmail's own API, Google contacts and calendars) until you build with a
  Google OAuth client of your own (see [BUILDING.md](BUILDING.md#a-google-oauth-client)). A
  Google client that hasn't passed Google's review signs in only the test users you list, for 7
  days at a time. Gmail still works over IMAP with a Google app password.
- **No automatic updates.** The update server is left out, so a build doesn't look for or install
  new versions until you set up your own (see
  [BUILDING.md](BUILDING.md#your-own-update-server)). Install a newer build yourself.
- **Not signed.** Windows SmartScreen warns about the installer (choose **More info ▸ Run
  anyway**). macOS refuses to open a downloaded app that isn't signed and notarized: after the
  first try, choose **Open Anyway** in System Settings ▸ Privacy & Security, or clear its
  quarantine flag with `xattr -dr com.apple.quarantine /Applications/Eudora.app`. The official Mac version is signed and notarized by Planet Networks; official
  Windows installers aren't code-signed yet either.
- **Linux packages don't add Eudora's apt or dnf repository**, so they aren't kept up to date by
  the system's updates.
- **Font Awesome Free icons** replace the Pro icons of official versions, so a few icons look
  different.

Everything else works the same: IMAP, POP3, and SMTP accounts, imports, search, the spam filter,
rules, Views, backups, the command line, the API, and plugins. Both kinds open the same library
(not at the same time), so you can switch between them; a Microsoft or Google sign-in belongs to
the app that made it, so after switching you sign in to those accounts again.

## Building

See **[BUILDING.md](BUILDING.md)**: setting up a computer (scripts for each platform), building,
testing, packaging, and the settings a build of your own needs (its own Microsoft and Google
sign-in registrations and update server).

## License

Eudora is free software by Planet Networks, under your choice of the
[MIT License](LICENSE-MIT) or the [Apache License 2.0](LICENSE-APACHE).
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) lists the work of others it uses.

Parts of Eudora, among them its classic icons and some of its classic behavior, are based on
classic Eudora, whose source the Computer History Museum released under a BSD license; its
copyright notice and license are in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). "Eudora"
here is a new program, not QUALCOMM's.

Icons: official builds use Font Awesome Pro (Planet Networks' license). The published source uses
the [Font Awesome Free](https://fontawesome.com) icons (CC BY 4.0) in their place, so it builds
without a Pro license; using the Pro icons in a build of your own needs a Font Awesome Pro license
of your own.

## Feedback

Bug reports and suggestions: **software@planet.net**. Eudora comes with no warranty and no
support, but the intention is to fix reported bugs quickly and to add good suggestions.
