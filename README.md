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
