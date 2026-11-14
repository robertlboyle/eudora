use super::*;
use crate::import::{ImportOptions, import};
use crate::library::Access;
use crate::marks::Status;
use crate::progress::Silent;
use crate::rules::Test;

const ME: &str = "pat@example.com";

fn mail(id: &str, subject: &str, date: &str, body: &str) -> String {
    format!("From: Ann Smith <ann@example.com>\r\nTo: Pat <{ME}>\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: <{id}>\r\n\r\n{body}")
}

fn with_attachment(id: &str) -> String {
    format!(
        "From: Ann Smith <ann@example.com>\r\nTo: {ME}\r\nSubject: The budget\r\nDate: Tue, 04 Jan 2000 10:00:00 -0500\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>See the <b>budget</b> attached.</p>\r\n\
         --b1\r\nContent-Type: application/pdf; name=\"Budget 2000.pdf\"\r\nContent-Disposition: attachment; filename=\"Budget 2000.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQKJSBmYWtlIHBkZgo=\r\n--b1--\r\n"
    )
}

fn import_into(library: &mut Library, dir: &Path, files: &[(&str, String)], folder: &str) {
    let src = dir.join(format!("src-{}", folder.replace('/', "-")));
    std::fs::create_dir_all(&src).unwrap();
    let mut paths = Vec::new();
    for (name, text) in files {
        let path = src.join(name);
        std::fs::write(&path, text).unwrap();
        paths.push(path);
    }
    let options = ImportOptions { destination: Some(folder.into()), account: Some(ME.into()), merge_account: Some(true), ..Default::default() };
    import(library, &paths, &options, &Silent).unwrap();
}

fn id_of(library: &Library, subject: &str) -> i64 {
    library.conn.query_row("SELECT id FROM messages WHERE subject = ?1", [subject], |r| r.get(0)).unwrap()
}

/// Every summary points at a message in its .mbx, and the .toc names the .mbx's size.
fn check_mailbox(mbx: &Path) -> Vec<Summary> {
    let data = std::fs::read(mbx).unwrap();
    let toc = std::fs::read(mbx.with_extension("toc")).unwrap();
    let (_, size, sums) = read_toc(&toc).expect("a .toc classic can read");
    assert_eq!(size, data.len() as u64, "{}: the .toc names the mailbox's size", mbx.display());
    for s in &sums {
        let (o, l) = (s.offset as usize, s.length as usize);
        assert!(o + l <= data.len());
        assert!(data[o..].starts_with(b"From ???@??? "), "{}: a summary starts at a From line", mbx.display());
        assert!(data[o..o + l].ends_with(b"\r\n"));
    }
    let mut ends: Vec<_> = sums.iter().map(|s| (s.offset, s.length)).collect();
    ends.sort();
    for w in ends.windows(2) {
        assert!(w[0].0 + w[0].1 <= w[1].0, "summaries don't overlap");
    }
    sums
}

fn library_with_mail(dir: &Path) -> Library {
    let mut library = Library::open(&dir.join("lib"), Access::Write, &Silent).unwrap();
    let date = "Fri, 31 Dec 1999 20:38:46 -0500";
    import_into(
        &mut library,
        dir,
        &[
            ("1.eml", mail("1@x", "Hello", date, "Hello there.\r\nFrom the start of a line.\r\n")),
            ("2.eml", mail("2@x", "Greek", date, "Καλημέρα κόσμε\r\n")),
            ("3.eml", with_attachment("3@x")),
        ],
        "Inbox",
    );
    import_into(&mut library, dir, &[("4.eml", mail("4@x", "Sent one", date, "I wrote this.\r\n"))], "Sent Items");
    import_into(&mut library, dir, &[("5.eml", mail("5@x", "Acme order", date, "An order.\r\n"))], "Clients/Acme");
    library
}

#[test]
fn a_toc_reads_back_as_written() {
    let sums = vec![Summary { offset: 0, length: 120, seconds: 946_690_726, tz_minutes: -300, state: MS_REPLIED, flags: MSF_HAS_ATTACHMENT, flags_ex: MSFEX_HTML, priority: 2, label: 3, date: "Fri, 31 Dec 1999".into(), from: "Ann Smith".into(), subject: "Café".into(), uid: 1, arrival: 946_690_726 }];
    let bytes = toc_bytes("In", 0, 120, &sums);
    assert_eq!(bytes.len(), TOC_HEADER_SIZE + SUMMARY_SIZE);
    let (name, size, back) = read_toc(&bytes).unwrap();
    assert_eq!((name.as_str(), size), ("In", 120));
    assert_eq!(back, sums);
}

#[test]
fn export_writes_a_classic_folder_and_reimports_the_same_mail_and_rules() {
    let dir = tempfile::tempdir().unwrap();
    let library = library_with_mail(dir.path());
    let hello = id_of(&library, "Hello");
    library.set_status(&[hello], Status::Unread).unwrap();
    let greek = id_of(&library, "Greek");
    library.set_status(&[greek], Status::Replied).unwrap();
    library.set_priority(&[greek], Some(1)).unwrap();
    let first_label = library.label_list().unwrap()[0].name.clone();
    library.set_label(&[greek], &first_label, true).unwrap();
    let account: i64 = library.conn.query_row("SELECT id FROM accounts WHERE address = ?1", [ME], |r| r.get(0)).unwrap();
    library
        .conn
        .execute(
            "INSERT INTO cards (account, card, updated) VALUES (?1, ?2, 0)",
            params![account, serde_json::to_string(&Card { name: "Ann Smith".into(), given: "Ann".into(), family: "Smith".into(), emails: vec![crate::cards::Labeled { kind: "work".into(), value: "ann@example.com".into() }], notes: "Met in 1999\nat the fair".into(), ..Default::default() }).unwrap()],
        )
        .unwrap();
    let test = |field: &str, verb: Verb, value: &str| Test { field: field.into(), verb, value: value.into(), unless: false };
    let rules = vec![
        Rule { name: "Acme".into(), enabled: true, incoming: true, all: true, tests: vec![test("From:", Verb::Contains, "acme.com")], actions: vec![Action::Transfer { account: Some(ME.into()), folder: "Clients/Acme".into() }, Action::Label { label: first_label.clone() }], ..Default::default() },
        Rule { name: "Either".into(), enabled: true, manual: true, all: false, tests: vec![test("Subject:", Verb::StartsWith, "Re"), test("«Any Recipient»", Verb::Is, "x@y")], actions: vec![Action::Priority { level: -1 }, Action::Stop], ..Default::default() },
        Rule { name: "Spam".into(), enabled: true, incoming: true, actions: vec![Action::Sort], ..Default::default() },
        Rule { name: "Three".into(), enabled: true, incoming: true, all: true, tests: vec![test("To:", Verb::Contains, "a"), test("To:", Verb::Contains, "b"), test("To:", Verb::Contains, "c")], actions: vec![Action::Delete], ..Default::default() },
    ];
    library.set_rules(&rules).unwrap();

    let out = dir.path().join("classic");
    let report = export(&library, &out, &ClassicOptions::default(), &Silent).unwrap();
    assert_eq!(report.written, 5);
    assert_eq!(report.attachments, 1);
    assert_eq!(report.as_html, 1, "the Greek text is kept as HTML");
    assert_eq!(report.rules_written, 2);
    assert_eq!(report.rules_left_out.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), vec!["Spam", "Three"]);
    assert_eq!(report.contacts, 1);
    for f in ["In.mbx", "Out.mbx", "Trash.mbx", "descmap.pce", "Filters.pce", "NNdbase.txt", "eudora.ini"] {
        assert!(out.join(f).is_file(), "{f} written");
    }
    let acme = out.join("Clients.fol").join("Acme.mbx");
    assert!(acme.is_file());
    let root_map = String::from_utf8(std::fs::read(out.join("descmap.pce")).unwrap()).unwrap();
    assert!(root_map.contains("In,In.mbx,S,U") && root_map.contains("Clients,Clients.fol,F,N"), "{root_map}");
    let fol_map = String::from_utf8(std::fs::read(out.join("Clients.fol").join("descmap.pce")).unwrap()).unwrap();
    assert_eq!(fol_map.trim(), "Acme,Acme.mbx,M,U");

    // the In box: three messages, their marks where classic keeps them
    let sums = check_mailbox(&out.join("In.mbx"));
    assert_eq!(sums.len(), 3);
    let hello_sum = sums.iter().find(|s| s.subject == "Hello").unwrap();
    assert_eq!(hello_sum.state, MS_UNREAD);
    let greek_sum = sums.iter().find(|s| s.subject == "Greek").unwrap();
    assert_eq!((greek_sum.state, greek_sum.priority, greek_sum.label), (MS_REPLIED, 1, 1));
    assert_eq!(greek_sum.tz_minutes, -300);
    let budget = sums.iter().find(|s| s.subject == "The budget").unwrap();
    assert!(budget.flags & MSF_HAS_ATTACHMENT != 0 && budget.flags_ex & MSFEX_HTML != 0);
    let data = std::fs::read(out.join("In.mbx")).unwrap();
    let text = String::from_utf8_lossy(&data);
    assert!(text.contains("\r\n>From the start of a line."), "a body line starting with From is escaped");
    assert!(text.contains("<x-html>") && text.contains("&#922;"), "Greek kept as character references");
    assert!(text.contains("Attachment Converted: \"") && text.contains("Budget 2000.pdf\""));
    assert!(out.join("Attach").join("Budget 2000.pdf").is_file());
    assert_eq!(check_mailbox(&out.join("Out.mbx"))[0].state, MS_SENT);

    // rules: classic's file read back by Eudora's own reader
    let filters = encoding_rs::WINDOWS_1252.decode(&std::fs::read(out.join("Filters.pce")).unwrap()).0.into_owned();
    let back = crate::rules::import_classic(&filters, ME).unwrap();
    assert_eq!(back.len(), 2);
    assert_eq!(back[0].tests, rules[0].tests);
    assert!(back[0].incoming && back[0].all);
    assert_eq!(back[0].actions, vec![Action::Transfer { account: Some(ME.into()), folder: "Clients.fol/Acme".into() }, Action::Label { label: "1".into() }]);
    assert_eq!(back[1].tests, rules[1].tests);
    assert!(back[1].manual && !back[1].all);
    assert_eq!(back[1].actions, vec![Action::Priority { level: -1 }, Action::Stop]);
    let nn = encoding_rs::WINDOWS_1252.decode(&std::fs::read(out.join("NNdbase.txt")).unwrap()).0.into_owned();
    assert!(nn.contains("alias \"Ann Smith\" ann@example.com\r\n") && nn.contains("<first:Ann>") && nn.contains("Met in 1999\u{3}at the fair"), "{nn}");

    // the mail read back by Eudora's own classic importer: the same messages in the same places
    let mut again = Library::open(&dir.path().join("lib2"), Access::Write, &Silent).unwrap();
    import(&mut again, &[out.clone()], &ImportOptions::default(), &Silent).unwrap();
    let place = |l: &Library, id: &str| -> Vec<String> {
        let m: i64 = l.conn.query_row("SELECT id FROM messages WHERE message_id = ?1", [id], |r| r.get(0)).unwrap();
        l.folder_paths(m).unwrap()
    };
    assert_eq!(place(&again, "1@x"), vec!["In".to_string()]);
    assert_eq!(place(&again, "4@x"), vec!["Out".to_string()]);
    assert_eq!(place(&again, "5@x"), vec!["Clients.fol/Acme".to_string()]);
    let count: i64 = again.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 5);
    for (id, subject) in [("1@x", "Hello"), ("2@x", "Greek"), ("3@x", "The budget")] {
        let s: String = again.conn.query_row("SELECT subject FROM messages WHERE message_id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(s, subject);
    }
    let m: i64 = again.conn.query_row("SELECT id FROM messages WHERE message_id = '1@x'", [], |r| r.get(0)).unwrap();
    let shown = crate::parse::display(&again.raw_message(m).unwrap(), now()).text;
    assert!(shown.contains("From the start of a line."), "unescaped again: {shown}");
    // classic's attachment line leads Eudora back to the decoded file
    let budget: i64 = again.conn.query_row("SELECT id FROM messages WHERE message_id = '3@x'", [], |r| r.get(0)).unwrap();
    assert!(String::from_utf8_lossy(&again.raw_message(budget).unwrap()).contains("Budget 2000.pdf"));
}

#[test]
fn an_update_adds_new_mail_takes_out_moved_mail_and_compacts() {
    let dir = tempfile::tempdir().unwrap();
    let mut library = library_with_mail(dir.path());
    let out = dir.path().join("classic");
    export(&library, &out, &ClassicOptions::default(), &Silent).unwrap();
    let in_size = std::fs::metadata(out.join("In.mbx")).unwrap().len();

    // nothing changed: nothing written
    let r = update(&library, &out, &ClassicOptions::default(), None, &Silent).unwrap();
    assert_eq!((r.written, r.removed, r.changed), (0, 0, 0));
    assert_eq!(std::fs::metadata(out.join("In.mbx")).unwrap().len(), in_size);

    // a message moved in Eudora leaves In and reaches Acme; a new one arrives; one is read
    let inbox = library.find_folder("Inbox").unwrap().unwrap();
    let acme = library.find_folder("Clients/Acme").unwrap().unwrap();
    let budget = id_of(&library, "The budget");
    library.move_messages(&[budget], inbox, acme).unwrap();
    import_into(&mut library, dir.path(), &[("6.eml", mail("6@x", "Brand new", "Sat, 01 Jan 2000 09:00:00 +0000", "New.\r\n"))], "Inbox");
    let r = update(&library, &out, &ClassicOptions::default(), Some(1), &Silent).unwrap();
    assert_eq!(r.written, 1, "a batch at a time");
    assert_eq!(r.left, 1);
    let r = update(&library, &out, &ClassicOptions::default(), None, &Silent).unwrap();
    assert_eq!(r.written, 1);
    assert_eq!(r.removed, 0, "taken out in the first batch");
    let in_sums = check_mailbox(&out.join("In.mbx"));
    assert_eq!(in_sums.iter().map(|s| s.subject.as_str()).collect::<Vec<_>>().len(), 3);
    assert!(in_sums.iter().all(|s| s.subject != "The budget"));
    assert!(check_mailbox(&out.join("Clients.fol").join("Acme.mbx")).iter().any(|s| s.subject == "The budget"));
    // the budget was the biggest message: over a quarter unused, so In was compacted
    let in_now = std::fs::read(out.join("In.mbx")).unwrap();
    let used: u64 = in_sums.iter().map(|s| u64::from(s.length)).sum();
    assert_eq!(used, in_now.len() as u64);

    // a folder no longer exported (deleted in Eudora, or left out): its mailbox goes
    let others: Vec<i64> = library.folders().unwrap().iter().map(|f| f.id).filter(|id| *id != acme).collect();
    update(&library, &out, &ClassicOptions { folders: others, ..Default::default() }, None, &Silent).unwrap();
    assert!(!out.join("Clients.fol").join("Acme.mbx").exists());
    assert!(!String::from_utf8_lossy(&std::fs::read(out.join("descmap.pce")).unwrap()).contains("Clients.fol"));
    assert!(!out.join("Clients.fol").exists());
}

#[test]
fn mail_changed_in_classic_is_kept_and_nothing_is_written_while_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let library = library_with_mail(dir.path());
    let out = dir.path().join("classic");
    export(&library, &out, &ClassicOptions::default(), &Silent).unwrap();

    // classic Eudora running: OWNER.LOK there (handle 0 = an instance without a window yet)
    std::fs::write(out.join("OWNER.LOK"), [0u8; 8]).unwrap();
    let r = update(&library, &out, &ClassicOptions::default(), None, &Silent).unwrap();
    assert!(r.waiting.is_some());
    std::fs::remove_file(out.join("OWNER.LOK")).unwrap();

    // mail fetched in classic Eudora: its In box is kept beside, Eudora's written again
    let mut f = OpenOptions::new().append(true).open(out.join("In.mbx")).unwrap();
    f.write_all(b"From ???@??? Mon Jan 03 01:05:34 2000\r\nSubject: fetched by classic\r\n\r\nhi\r\n").unwrap();
    drop(f);
    let r = update(&library, &out, &ClassicOptions::default(), None, &Silent).unwrap();
    assert_eq!(r.kept_aside.len(), 1);
    assert!(std::fs::read(&r.kept_aside[0]).map(|b| String::from_utf8_lossy(&b).contains("fetched by classic")).unwrap());
    assert_eq!(r.written, 3);
    assert_eq!(check_mailbox(&out.join("In.mbx")).len(), 3);
}

#[test]
fn a_folder_with_other_files_or_inside_dropbox_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let library = library_with_mail(dir.path());
    let busy = dir.path().join("busy");
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("notes.txt"), b"x").unwrap();
    assert!(export(&library, &busy, &ClassicOptions::default(), &Silent).is_err());
    assert!(export(&library, &dir.path().join("Dropbox").join("old"), &ClassicOptions::default(), &Silent).is_err());
    assert!(export(&library, &dir.path().join("lib").join("x"), &ClassicOptions::default(), &Silent).is_err());
}

#[test]
fn a_mailbox_too_big_for_classic_carries_on_in_parts() {
    let dir = tempfile::tempdir().unwrap();
    let library = library_with_mail(dir.path());
    let out = dir.path().join("classic");
    // a limit of 600 bytes stands in for classic's 2 GB: about one message per file
    let r = update_with(&library, &out, &ClassicOptions::default(), None, 600, &Silent).unwrap();
    assert_eq!(r.written, 5);
    let map = String::from_utf8_lossy(&std::fs::read(out.join("descmap.pce")).unwrap()).into_owned();
    assert!(map.contains("In (2),In (2).mbx,M,U"), "{map}");
    // every Inbox message once, across In and its parts; each file valid and within the limit
    // unless it holds a single message
    let mut subjects = Vec::new();
    for name in ["In.mbx", "In (2).mbx", "In (3).mbx"] {
        let mbx = out.join(name);
        if !mbx.exists() {
            continue;
        }
        let sums = check_mailbox(&mbx);
        assert!(sums.len() == 1 || std::fs::metadata(&mbx).unwrap().len() <= 600);
        subjects.extend(sums.into_iter().map(|s| s.subject));
    }
    subjects.sort();
    assert_eq!(subjects, vec!["Greek", "Hello", "The budget"]);
    // nothing changed: nothing written, the parts stay
    let again = update_with(&library, &out, &ClassicOptions::default(), None, 600, &Silent).unwrap();
    assert_eq!((again.written, again.removed), (0, 0));
    // a message deleted in Eudora leaves whichever part holds it
    let hello = id_of(&library, "Hello");
    library.conn.execute("DELETE FROM locations WHERE message = ?1", [hello]).unwrap();
    let after = update_with(&library, &out, &ClassicOptions::default(), None, 600, &Silent).unwrap();
    assert_eq!(after.removed, 1);
    // read back by Eudora's importer: In and "In (2)" are mailboxes like any other
    let mut back = Library::open(&dir.path().join("lib2"), Access::Write, &Silent).unwrap();
    import(&mut back, &[out.clone()], &ImportOptions::default(), &Silent).unwrap();
    let n: i64 = back.conn.query_row("SELECT COUNT(*) FROM messages WHERE message_id IN ('2@x', '3@x')", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);
}
