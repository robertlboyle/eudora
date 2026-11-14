//! Contacts and calendars on CardDAV and CalDAV servers: iCloud, Fastmail, Nextcloud, Yahoo,
//! hosting companies' servers (cPanel, SmarterMail, SabreDAV), two-way like Microsoft's and
//! Google's.
//!
//! **An account** is an address with its server (found from the address, or typed), a user name
//! and a password (an app password for iCloud and Fastmail; else the mail password, which is
//! offered when an IMAP account of the same address has one). Its settings are kept in the
//! library (`dav-account:<address>`), the password on this computer (`dav:<address>`). It hangs
//! on the IMAP / POP3 account of the same address, else on an account of its own (kind `dav`).
//!
//! **Finding the server** (RFC 6764 and RFC 5397): the address's well-known addresses
//! (`/.well-known/carddav`, `/caldav`, on the domain and on `mail.<domain>`) or the server typed,
//! asked for the user's principal; the principal names the homes of the address books and
//! calendars. A server that answers only one well-known address is asked for both homes there.
//!
//! **Reading** (the IMAP side's change round, as Google's, `gmail_changes`): the address books
//! hourly (listed, then read in hundreds with `addressbook-multiget`), the calendars' near weeks
//! every 15 minutes and everything daily (`calendar-query` with the server expanding repeating
//! events, so each occurrence is its own event, as Microsoft gives them). Events are put in the
//! shape Eudora reads Microsoft's (`calendar::record_calendars`), cards in Eudora's own.
//!
//! **Changes** go through the same queue (`pending_ops` card-* and event-change): a contact or
//! event written with `If-Match` on its ETag (a newer copy on the server is never overwritten:
//! the change fails with why, and the next read brings the server's copy), made with
//! `If-None-Match: *`, deleted. An edited vCard keeps every line Eudora doesn't edit (photos,
//! other programs' fields); an edited event keeps everything but what was changed. Answering an
//! invitation sets the user's PARTSTAT; the server tells the organizer (CalDAV scheduling, RFC
//! 6638), as iCloud, Fastmail and SabreDAV do.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::library::Library;

/// The settings of an account's contacts and calendars server.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DavConfig {
    pub address: String,
    /// The user name ("" = the address).
    #[serde(default)]
    pub user: String,
    /// The server as typed ("" = found from the address).
    #[serde(default)]
    pub server: String,
    /// Where the address books and calendars are (found when the account is saved).
    #[serde(default)]
    pub contacts: Option<String>,
    #[serde(default)]
    pub calendars: Option<String>,
}

impl DavConfig {
    pub fn user(&self) -> &str {
        if self.user.trim().is_empty() { &self.address } else { self.user.trim() }
    }
}

const SETTINGS_PREFIX: &str = "dav-account:";

fn secret_name(address: &str) -> String {
    format!("dav:{}", address.to_lowercase())
}

/// The password kept for an account's contacts and calendars (else its mail password, when the
/// same address has an IMAP / POP3 account: one password for both on most servers).
pub fn password(address: &str) -> Option<String> {
    crate::graph::load_secret(&secret_name(address))
        .or_else(|| crate::graph::load_secret(&format!("imap:{}", address.to_lowercase())))
        .and_then(|b| String::from_utf8(b).ok())
}

// ------------------------------------------------------------------ XML

/// An XML element: its local name (lower case, no namespace), attributes, text, and children.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    /// The first element of that name anywhere below.
    pub fn find(&self, name: &str) -> Option<&Node> {
        self.children.iter().find_map(|c| if c.name == name { Some(c) } else { c.find(name) })
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The `href` inside (current-user-principal, a home set).
    pub fn href(&self) -> Option<String> {
        self.find("href").map(|h| h.text.trim().to_string()).filter(|h| !h.is_empty())
    }
}

/// Reads an XML document into its root element (namespaces dropped: DAV's few names don't
/// clash). None when it isn't XML.
pub fn parse_xml(text: &str) -> Option<Node> {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    fn node(e: &quick_xml::events::BytesStart) -> Node {
        let name = e.local_name().as_ref().to_ascii_lowercase();
        let attrs = e
            .attributes()
            .flatten()
            .map(|a| (a.key.local_name().as_ref().to_ascii_lowercase(), a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map(|v| v.to_string()).unwrap_or_default()))
            .collect();
        Node { name, attrs, ..Default::default() }
    }
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Node> = vec![Node::default()];
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => stack.push(node(&e)),
            Ok(Event::Empty(e)) => {
                let n = node(&e);
                stack.last_mut()?.children.push(n);
            }
            Ok(Event::End(_)) => {
                if stack.len() < 2 {
                    return None;
                }
                let n = stack.pop()?;
                stack.last_mut()?.children.push(n);
            }
            Ok(Event::Text(t)) => stack.last_mut()?.text.push_str(&t.xml10_content()),
            Ok(Event::CData(c)) => stack.last_mut()?.text.push_str(&c.xml10_content()),
            Ok(Event::GeneralRef(r)) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c.to_string()),
                    _ => match &*r {
                        "amp" => Some("&".into()),
                        "lt" => Some("<".into()),
                        "gt" => Some(">".into()),
                        "quot" => Some("\"".into()),
                        "apos" => Some("'".into()),
                        _ => None,
                    },
                };
                if let Some(s) = resolved {
                    stack.last_mut()?.text.push_str(&s);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    stack.into_iter().next()?.children.into_iter().next()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// One resource of a multistatus answer: its href and the properties the server had (200).
#[derive(Clone, Debug)]
pub struct DavItem {
    pub href: String,
    pub props: Node,
}

impl DavItem {
    fn text(&self, name: &str) -> Option<String> {
        self.props.child(name).map(|n| n.text.trim().to_string()).filter(|t| !t.is_empty())
    }

    /// Whether `resourcetype` holds an element of that name (collection, addressbook, calendar).
    fn is(&self, kind: &str) -> bool {
        self.props.child("resourcetype").is_some_and(|r| r.child(kind).is_some())
    }
}

fn multistatus(text: &str) -> Result<Vec<DavItem>> {
    let root = parse_xml(text).ok_or_else(|| Error::Other("the server's answer isn't XML".into()))?;
    let mut out = Vec::new();
    for r in root.children.iter().filter(|c| c.name == "response") {
        let Some(href) = r.child("href").map(|h| h.text.trim().to_string()) else { continue };
        let mut props = Node { name: "prop".into(), ..Default::default() };
        for ps in r.children.iter().filter(|c| c.name == "propstat") {
            let ok = ps.child("status").map(|s| s.text.contains(" 200")).unwrap_or(true);
            if ok {
                if let Some(p) = ps.child("prop") {
                    props.children.extend(p.children.iter().cloned());
                }
            }
        }
        out.push(DavItem { href, props });
    }
    Ok(out)
}

// ------------------------------------------------------------------ the client

/// An answer: its status, the address it finally came from, its ETag, and its body.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub url: String,
    pub etag: Option<String>,
    pub body: String,
}

/// A connection to one account's server (Basic authentication, over HTTPS only).
pub struct DavClient {
    agent: ureq::Agent,
    auth: String,
}

/// `href` read against the address it came from.
pub fn resolve(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = base[scheme_end..].find('/').map(|i| scheme_end + i).unwrap_or(base.len());
    if href.starts_with('/') {
        return format!("{}{href}", &base[..host_end]);
    }
    let dir = match base[host_end..].rfind('/') {
        Some(i) => &base[..host_end + i + 1],
        None => base,
    };
    format!("{dir}{href}")
}

/// The path of an address (what a server's hrefs usually are).
fn path_of(url: &str) -> String {
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    url[scheme_end..].find('/').map(|i| url[scheme_end + i..].to_string()).unwrap_or_else(|| "/".into())
}

/// Two hrefs for the same resource ("/a/b/" and "https://host/a/b", percent-encoded or not).
fn same_href(a: &str, b: &str) -> bool {
    let norm = |h: &str| percent_decode(&path_of(&resolve("https://x/", h))).trim_end_matches('/').to_lowercase();
    norm(a) == norm(b)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl DavClient {
    pub fn new(user: &str, password: &str) -> DavClient {
        use base64::Engine;
        let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            // (followed here, so the password goes along when iCloud sends address books on to
            // another of its servers, and the method and body stay the same)
            .max_redirects(0)
            // (a new connection each request: servers close theirs after some answers, and a
            // reused one then failed with "peer disconnected", 2026-11-11, SabreDAV)
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .timeout_global(Some(std::time::Duration::from_secs(300)))
            .timeout_connect(Some(std::time::Duration::from_secs(20)))
            .timeout_recv_response(Some(std::time::Duration::from_secs(90)))
            .user_agent(concat!("Eudora/", env!("CARGO_PKG_VERSION")))
            .tls_config(tls)
            .build();
        let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}")));
        DavClient { agent: ureq::Agent::new_with_config(config), auth }
    }

    /// A request, following up to six redirects (same method, body, and password).
    pub fn send(&self, method: &str, url: &str, headers: &[(&str, &str)], body: Option<&str>) -> Result<Answer> {
        let mut url = url.to_string();
        for _ in 0..7 {
            if !url.starts_with("https://") {
                return Err(Error::Other(format!("{url}: only https:// addresses are used (the password would travel unencrypted)")));
            }
            // (uncompressed: cPanel's server marks an empty answer to DELETE as gzip, which no
            // reader can open)
            let mut b = ureq::http::Request::builder().method(method).uri(&url).header("Authorization", &self.auth).header("Accept-Encoding", "identity");
            for (k, v) in headers {
                b = b.header(*k, *v);
            }
            let request = match body {
                Some(text) => self.agent.run(b.body(text.to_string()).map_err(|e| Error::Other(format!("{url}: {e}")))?),
                None => self.agent.run(b.body(()).map_err(|e| Error::Other(format!("{url}: {e}")))?),
            };
            let mut resp = request.map_err(crate::graph::net)?;
            let status = resp.status().as_u16();
            let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if let Some(to) = header("location") {
                    // (the redirect's own answer read first: a connection left with it unread
                    // was dropped by the server when used again)
                    let _ = resp.body_mut().with_config().limit(1 << 20).read_to_vec();
                    url = resolve(&url, &to);
                    continue;
                }
            }
            let etag = header("etag");
            let text = match resp.body_mut().with_config().limit(64 << 20).read_to_string() {
                Ok(t) => t,
                // a change done (PUT, DELETE…) whose empty answer can't be read is still done
                Err(_) if status < 300 && !matches!(method, "GET" | "PROPFIND" | "REPORT") => String::new(),
                Err(e) => return Err(crate::graph::net(e)),
            };
            return match status {
                401 => Err(Error::Other(format!("{}: the server refused the user name or password", host_of(&url)))),
                403 => Err(Error::Other(format!("{}: the server doesn't allow this (403 Forbidden)", host_of(&url)))),
                _ => Ok(Answer { status, url, etag, body: text }),
            };
        }
        Err(Error::Other(format!("{url}: too many redirects")))
    }

    fn checked(&self, method: &str, url: &str, headers: &[(&str, &str)], body: Option<&str>) -> Result<Answer> {
        let a = self.send(method, url, headers, body)?;
        if a.status >= 400 {
            return Err(Error::Other(format!("{} {}: the server answered {}", method, a.url, a.status)));
        }
        Ok(a)
    }

    /// PROPFIND: the properties asked for (`props`: elements inside <d:prop>), of `url` and (depth
    /// 1) what's inside it. Returns the items and the address that answered.
    pub fn propfind(&self, url: &str, depth: &str, props: &str) -> Result<(Vec<DavItem>, String)> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ical="http://apple.com/ns/ical/"><d:prop>{props}</d:prop></d:propfind>"#
        );
        let a = self.checked("PROPFIND", url, &[("Depth", depth), ("Content-Type", "application/xml; charset=utf-8")], Some(&body))?;
        Ok((multistatus(&a.body)?, a.url))
    }

    /// The item's version on the server now (its ETag), if the server says.
    pub fn etag_of(&self, url: &str) -> Result<Option<String>> {
        match self.propfind(url, "0", "<d:getetag/>") {
            Ok((items, _)) => Ok(items.first().and_then(|i| i.text("getetag"))),
            Err(e) if gone(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn report(&self, url: &str, depth: &str, body: &str) -> Result<Vec<DavItem>> {
        let a = self.checked("REPORT", url, &[("Depth", depth), ("Content-Type", "application/xml; charset=utf-8")], Some(body))?;
        multistatus(&a.body)
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split('/').next().unwrap_or(rest).to_string()
}

// ------------------------------------------------------------------ finding the server

/// Where an address's contacts and calendars live, as found.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Found {
    pub contacts: Option<String>,
    pub calendars: Option<String>,
    pub books: Vec<Collection>,
    pub calendar_list: Vec<Collection>,
}

/// An address book or a calendar.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Collection {
    pub href: String,
    pub name: String,
    pub color: Option<String>,
    pub writable: bool,
}

/// The addresses to ask first for an account's server: the one typed, the big providers'
/// own, then the domain's well-known addresses (on it and on mail.<domain>, and on the IMAP
/// server of the same address).
fn starts(config: &DavConfig, imap_host: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let server = config.server.trim().trim_end_matches('/');
    if !server.is_empty() {
        let s = if server.contains("://") { server.to_string() } else { format!("https://{server}") };
        out.push(format!("{s}/"));
        out.push(format!("{s}/.well-known/caldav"));
        out.push(format!("{s}/.well-known/carddav"));
        // (cPanel serves contacts and calendars on a port of their own: the server typed without one)
        if !s.split("://").nth(1).unwrap_or("").contains(':') {
            out.push(format!("{s}:2080/"));
        }
    }
    let domain = config.address.rsplit('@').next().unwrap_or("").to_lowercase();
    match domain.as_str() {
        "icloud.com" | "me.com" | "mac.com" => {
            out.push("https://contacts.icloud.com/".into());
            out.push("https://caldav.icloud.com/".into());
        }
        d if d.contains("fastmail") || d == "messagingengine.com" => {
            out.push("https://carddav.fastmail.com/".into());
            out.push("https://caldav.fastmail.com/".into());
        }
        "yahoo.com" | "ymail.com" | "rocketmail.com" => {
            out.push("https://carddav.address.yahoo.com/".into());
            out.push("https://caldav.calendar.yahoo.com/".into());
        }
        _ => {}
    }
    let mut hosts = vec![domain.clone(), format!("mail.{domain}")];
    if let Some(h) = imap_host.map(str::to_lowercase).filter(|h| !hosts.contains(h)) {
        hosts.push(h);
    }
    for h in hosts.iter().filter(|h| !h.is_empty()) {
        out.push(format!("https://{h}/.well-known/carddav"));
        out.push(format!("https://{h}/.well-known/caldav"));
    }
    // last, cPanel's own port on the mail server (one try: a closed port waits out its timeout)
    let mail_host = imap_host.map(str::to_lowercase).unwrap_or_else(|| format!("mail.{domain}"));
    if !domain.is_empty() {
        out.push(format!("https://{mail_host}:2080/"));
    }
    out.dedup();
    out
}

const HOMES: &str = "<d:current-user-principal/><card:addressbook-home-set/><cal:calendar-home-set/>";

/// Finds an account's address books and calendars (the server asked as `starts` says). Errors
/// only when nothing answered as a CardDAV / CalDAV server (a refused password says so).
/// `discover` with the password typed, else the one kept for the address (or its IMAP account's).
pub fn find_dav(config: &DavConfig, password: Option<&str>, imap_host: Option<&str>) -> Result<Found> {
    let password = match password.filter(|p| !p.is_empty()) {
        Some(p) => p.to_string(),
        None => self::password(&config.address).ok_or_else(|| Error::Other(format!("type the password for {} (an app password for iCloud and Fastmail)", config.address)))?,
    };
    discover(config, &password, imap_host)
}

pub fn discover(config: &DavConfig, password: &str, imap_host: Option<&str>) -> Result<Found> {
    let client = DavClient::new(config.user(), password);
    let mut found = Found { contacts: config.contacts.clone(), calendars: config.calendars.clone(), ..Default::default() };
    let mut refused = None;
    // (what each address answered, for the message when nothing was found)
    let mut tried: Vec<String> = Vec::new();
    let mut principals = Vec::new();
    if found.contacts.is_none() || found.calendars.is_none() {
        for start in starts(config, imap_host) {
            if start.ends_with(":2080/") && !port_open(&start) {
                tried.push(format!("{start}: nothing answers there"));
                continue;
            }
            match client.propfind(&start, "0", HOMES) {
                Ok((items, url)) => {
                    let Some(item) = items.first() else {
                        tried.push(format!("{start}: no answer about the user"));
                        continue;
                    };
                    if item.props.child("current-user-principal").is_none() && item.props.child("addressbook-home-set").is_none() && item.props.child("calendar-home-set").is_none() {
                        tried.push(format!("{start}: answered without the user's principal"));
                    }
                    let home = |name: &str| item.props.child(name).and_then(Node::href).map(|h| resolve(&url, &h));
                    found.contacts = found.contacts.take().or_else(|| home("addressbook-home-set"));
                    found.calendars = found.calendars.take().or_else(|| home("calendar-home-set"));
                    if let Some(p) = item.props.child("current-user-principal").and_then(Node::href) {
                        let p = resolve(&url, &p);
                        if !principals.contains(&p) {
                            principals.push(p);
                        }
                    }
                }
                Err(e) if e.to_string().contains("refused the user name or password") => {
                    tried.push(format!("{start}: the password was refused"));
                    refused = Some(e);
                }
                Err(e) => tried.push(format!("{start}: {}", e.to_string().rsplit(": ").next().unwrap_or("no answer"))),
            }
            if found.contacts.is_some() && found.calendars.is_some() {
                break;
            }
            // the principal names both homes (a server answering only one well-known address)
            for p in principals.clone() {
                if let Ok((items, url)) = client.propfind(&p, "0", HOMES) {
                    if let Some(item) = items.first() {
                        let home = |name: &str| item.props.child(name).and_then(Node::href).map(|h| resolve(&url, &h));
                        found.contacts = found.contacts.take().or_else(|| home("addressbook-home-set"));
                        found.calendars = found.calendars.take().or_else(|| home("calendar-home-set"));
                    }
                }
            }
            if found.contacts.is_some() && found.calendars.is_some() {
                break;
            }
        }
    }
    if let Some(home) = &found.contacts {
        found.books = collections(&client, home, "addressbook")?;
    }
    if let Some(home) = &found.calendars {
        found.calendar_list = collections(&client, home, "calendar")?;
    }
    if found.contacts.is_none() && found.calendars.is_none() {
        return Err(refused.unwrap_or_else(|| {
            Error::Other(format!(
                "no contacts or calendar server was found for {}: type its address in Server (your provider's help says it). Tried: {}",
                config.address,
                tried.join("; ")
            ))
        }));
    }
    Ok(found)
}

/// The address books ("addressbook") or calendars ("calendar") in a home.
pub fn collections(client: &DavClient, home: &str, kind: &str) -> Result<Vec<Collection>> {
    let (items, url) = client.propfind(
        home,
        "1",
        "<d:resourcetype/><d:displayname/><d:current-user-privilege-set/><ical:calendar-color/><cal:supported-calendar-component-set/>",
    )?;
    let mut out = Vec::new();
    for item in items.iter().filter(|i| i.is(kind)) {
        // (a calendar that holds only tasks or journals isn't one Eudora shows)
        if kind == "calendar" {
            if let Some(set) = item.props.child("supported-calendar-component-set") {
                if !set.children.is_empty() && !set.children.iter().any(|c| c.attr("name").is_some_and(|n| n.eq_ignore_ascii_case("VEVENT"))) {
                    continue;
                }
            }
        }
        let writable = match item.props.child("current-user-privilege-set") {
            Some(set) => set.children.iter().any(|p| p.children.iter().any(|q| matches!(q.name.as_str(), "write" | "write-content" | "all"))),
            None => true,
        };
        let href = resolve(&url, &item.href);
        let name = item.text("displayname").unwrap_or_else(|| percent_decode(href.trim_end_matches('/').rsplit('/').next().unwrap_or("")));
        // (Apple's colors are #RRGGBBAA)
        let color = item.text("calendar-color").map(|c| if c.len() == 9 && c.starts_with('#') { c[..7].to_string() } else { c });
        out.push(Collection { href, name, color, writable });
    }
    Ok(out)
}

// ------------------------------------------------------------------ contacts

/// An address book's contacts: (href, ETag, the vCard) as {id, etag, changeKey, vcard}.
pub fn contacts(client: &DavClient, book: &str) -> Result<Vec<Value>> {
    let (items, url) = client.propfind(book, "1", "<d:getetag/><d:getcontenttype/><d:resourcetype/>")?;
    let hrefs: Vec<String> = items
        .iter()
        .filter(|i| !same_href(&i.href, book) && !i.is("collection"))
        .filter(|i| i.text("getcontenttype").is_none_or(|t| t.to_lowercase().contains("vcard")) || i.href.to_lowercase().ends_with(".vcf"))
        .map(|i| i.href.clone())
        .collect();
    let mut out = Vec::new();
    for chunk in hrefs.chunks(100) {
        let refs: String = chunk.iter().map(|h| format!("<d:href>{}</d:href>", xml_escape(h))).collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/><card:address-data/></d:prop>{refs}</card:addressbook-multiget>"#
        );
        for item in client.report(&url, "1", &body)? {
            let (Some(vcard), Some(etag)) = (item.text("address-data"), item.text("getetag")) else { continue };
            out.push(json!({ "id": path_of(&resolve(&url, &item.href)), "etag": etag, "changeKey": etag, "vcard": vcard }));
        }
    }
    Ok(out)
}

/// The properties Eudora edits in a vCard (the others are kept as the server has them).
const EDITED: &[&str] = &["FN", "N", "NICKNAME", "ORG", "TITLE", "EMAIL", "TEL", "ADR", "URL", "BDAY", "NOTE", "CATEGORIES"];

/// A card as a vCard for its server: Eudora's fields, and every line of `original` (the copy
/// on the server) Eudora doesn't edit (photos, other programs' fields, its UID).
pub fn vcard_for(card: &crate::cards::Card, original: Option<&str>, uid: &str) -> String {
    let made = crate::cards::to_vcard(card);
    let unfold = |text: &str| -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for raw in text.split('\n') {
            let raw = raw.trim_end_matches('\r');
            if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
                lines.last_mut().expect("checked").push_str(&raw[1..]);
            } else if !raw.is_empty() {
                lines.push(raw.to_string());
            }
        }
        lines
    };
    let name_of = |line: &str| -> String {
        let head = line.split([':', ';']).next().unwrap_or("");
        head.rsplit('.').next().unwrap_or(head).to_ascii_uppercase()
    };
    let mut out: Vec<String> = unfold(&made).into_iter().filter(|l| !matches!(name_of(l).as_str(), "END" | "UID")).collect();
    let mut has_uid = false;
    if let Some(original) = original {
        for line in unfold(original) {
            let name = name_of(&line);
            if name == "UID" {
                has_uid = true;
            }
            if matches!(name.as_str(), "BEGIN" | "END" | "VERSION" | "PRODID" | "REV") || EDITED.contains(&name.as_str()) {
                continue;
            }
            out.push(line);
        }
    }
    if !has_uid {
        out.push(format!("UID:{uid}"));
    }
    out.push("END:VCARD".into());
    out.iter().map(|l| crate::ics::fold(l)).collect()
}

// ------------------------------------------------------------------ calendars

fn utc_stamp(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y%m%dT%H%M%SZ").to_string()).unwrap_or_default()
}

fn iso(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string()).unwrap_or_default()
}

/// A calendar's events from `from` to `to` (Unix seconds), each occurrence of a repeating one on
/// its own (the server expands them), in the shape Eudora reads Microsoft's.
pub fn events(client: &DavClient, calendar: &str, from: i64, to: i64, me: &str) -> Result<Vec<Value>> {
    let (start, end) = (utc_stamp(from), utc_stamp(to));
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><cal:calendar-query xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><cal:calendar-data><cal:expand start="{start}" end="{end}"/></cal:calendar-data></d:prop><cal:filter><cal:comp-filter name="VCALENDAR"><cal:comp-filter name="VEVENT"><cal:time-range start="{start}" end="{end}"/></cal:comp-filter></cal:comp-filter></cal:filter></cal:calendar-query>"#
    );
    let mut out = Vec::new();
    for item in client.report(calendar, "1", &body)? {
        let Some(data) = item.text("calendar-data") else { continue };
        let etag = item.text("getetag").unwrap_or_default();
        let href = path_of(&resolve(calendar, &item.href));
        let file = crate::ics::read(&data);
        let series = file.events.len() > 1 || file.events.iter().any(|e| e.recurrence_id.is_some() || e.repeats.is_some());
        // (a server that sends the rule, not the meetings (cPanel's): Eudora works them out)
        let unexpanded = file.events.iter().any(|e| e.repeats.is_some() && e.recurrence_id.is_none());
        let events = if unexpanded { crate::ics::expand(&data, from, to) } else { file.events };
        for e in &events {
            out.push(event_as_graph(e, &href, &etag, me, series));
        }
    }
    Ok(out)
}

/// A calendar in the shape Eudora reads Microsoft's.
pub fn calendar_as_graph(c: &Collection, first: bool) -> Value {
    json!({ "id": path_of(&c.href), "name": c.name, "hexColor": c.color, "canEdit": c.writable, "isDefaultCalendar": first })
}

/// A PARTSTAT as Graph's word for it.
fn answer_as_graph(partstat: &str) -> &'static str {
    match partstat {
        "ACCEPTED" => "accepted",
        "DECLINED" => "declined",
        "TENTATIVE" => "tentativelyAccepted",
        _ => "notResponded",
    }
}

/// An event read from a server in the shape Eudora reads Microsoft's (`calendar::from_graph`):
/// its id the resource's href (and "#" + its RECURRENCE-ID for one occurrence of a series).
pub fn event_as_graph(e: &crate::ics::IcsEvent, href: &str, etag: &str, me: &str, series: bool) -> Value {
    let me = me.to_lowercase();
    let id = match &e.recurrence_id {
        Some(rid) => format!("{href}#{rid}"),
        None if series && e.repeats.is_none() => href.to_string(),
        None => href.to_string(),
    };
    let organizer_is_me = e.organizer.as_ref().is_none_or(|(_, a)| a.eq_ignore_ascii_case(&me));
    let mine = e.attendees.iter().position(|(_, a)| a.eq_ignore_ascii_case(&me));
    let response = match (organizer_is_me, mine) {
        (true, _) => "organizer",
        (false, Some(i)) => answer_as_graph(e.answers.get(i).map(String::as_str).unwrap_or("")),
        (false, None) => "none",
    };
    let attendees: Vec<Value> = e
        .attendees
        .iter()
        .enumerate()
        .map(|(i, (name, address))| json!({ "emailAddress": { "name": name, "address": address }, "type": "required", "status": { "response": answer_as_graph(e.answers.get(i).map(String::as_str).unwrap_or("")) } }))
        .collect();
    json!({
        "id": id,
        "changeKey": format!("{etag}{}", e.recurrence_id.as_deref().map(|r| format!("#{r}")).unwrap_or_default()),
        "iCalUId": e.uid,
        "subject": e.summary,
        "start": { "dateTime": iso(e.start), "timeZone": "UTC" },
        "end": { "dateTime": iso(e.end), "timeZone": "UTC" },
        "isAllDay": e.all_day,
        "location": { "displayName": e.location },
        "organizer": { "emailAddress": { "name": e.organizer.as_ref().map(|o| o.0.clone()).unwrap_or_default(), "address": e.organizer.as_ref().map(|o| o.1.clone()).unwrap_or_default() } },
        "attendees": attendees,
        "isOrganizer": organizer_is_me,
        "responseStatus": { "response": response },
        "showAs": if e.free { "free" } else { "busy" },
        "isReminderOn": false,
        "sensitivity": if e.private { "private" } else { "normal" },
        "isCancelled": e.status.as_deref() == Some("CANCELLED"),
        "type": if e.recurrence_id.is_some() || (series && e.repeats.is_some()) { "occurrence" } else { "singleInstance" },
        "seriesMasterId": if e.recurrence_id.is_some() || e.repeats.is_some() { Value::from(href) } else { Value::Null },
        "bodyPreview": crate::text_repair::clean(&e.description, 2000),
        "body": { "contentType": "text", "content": e.description },
    })
}

// ------------------------------------------------------------------ writing events

/// A text from Eudora's event editor (HTML) as a calendar's plain text.
fn plain(html: &str) -> String {
    if html.contains('<') { mail_parser::decoders::html::html_to_text(html).trim().to_string() } else { html.trim().to_string() }
}

/// DTSTART / DTEND lines for a Graph time ({dateTime, timeZone: UTC}).
fn time_line(name: &str, v: &Value, all_day: bool) -> Option<String> {
    let text = v["dateTime"].as_str()?;
    let t = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").or_else(|_| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")).ok()?;
    Some(if all_day { format!("{name};VALUE=DATE:{}", t.format("%Y%m%d")) } else { format!("{name}:{}", t.format("%Y%m%dT%H%M%SZ")) })
}

/// The property lines a Graph change (`patch`) sets in an event, by name (removed first, then
/// these written).
fn lines_from_patch(p: &Value) -> Vec<(&'static str, Vec<String>)> {
    let mut out: Vec<(&'static str, Vec<String>)> = Vec::new();
    if let Some(s) = p["subject"].as_str() {
        out.push(("SUMMARY", vec![format!("SUMMARY:{}", crate::ics::escape(s))]));
    }
    if let Some(c) = p["body"]["content"].as_str() {
        let text = plain(c);
        out.push(("DESCRIPTION", if text.is_empty() { vec![] } else { vec![format!("DESCRIPTION:{}", crate::ics::escape(&text))] }));
    }
    if let Some(l) = p["location"]["displayName"].as_str() {
        out.push(("LOCATION", if l.trim().is_empty() { vec![] } else { vec![format!("LOCATION:{}", crate::ics::escape(l.trim()))] }));
    }
    let all_day = p["isAllDay"].as_bool().unwrap_or(false);
    if let Some(s) = time_line("DTSTART", &p["start"], all_day) {
        out.push(("DTSTART", vec![s]));
        out.push(("DURATION", vec![]));
    }
    if let Some(e) = time_line("DTEND", &p["end"], all_day) {
        out.push(("DTEND", vec![e]));
    }
    if let Some(s) = p["showAs"].as_str() {
        out.push(("TRANSP", vec![format!("TRANSP:{}", if s == "free" { "TRANSPARENT" } else { "OPAQUE" })]));
    }
    if let Some(s) = p["sensitivity"].as_str() {
        out.push(("CLASS", vec![format!("CLASS:{}", if s == "normal" { "PUBLIC" } else { "PRIVATE" })]));
    }
    out
}

/// A new event as a calendar file (the user its organizer when it has people: the server sends
/// the invitations).
pub fn new_event_ics(uid: &str, p: &Value, me: &str, my_name: &str) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".into(),
        concat!("PRODID:-//Planet Networks//Eudora ", env!("CARGO_PKG_VERSION"), "//EN").into(),
        "BEGIN:VEVENT".into(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{}", utc_stamp(crate::library::now())),
        "SEQUENCE:0".into(),
    ];
    for (_, set) in lines_from_patch(p) {
        lines.extend(set);
    }
    let people: Vec<(String, String)> = p["attendees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["emailAddress"]["address"].as_str().map(|ad| (a["emailAddress"]["name"].as_str().unwrap_or("").to_string(), ad.to_string())))
        .collect();
    if !people.is_empty() {
        lines.push(format!("ORGANIZER;CN=\"{}\":mailto:{me}", my_name.replace('"', "")));
        for (name, address) in people {
            lines.push(format!("ATTENDEE;CN=\"{}\";ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:{address}", name.replace('"', "")));
        }
    }
    if p["isReminderOn"].as_bool().unwrap_or(false) {
        let minutes = p["reminderMinutesBeforeStart"].as_i64().unwrap_or(15);
        lines.extend(["BEGIN:VALARM".to_string(), "ACTION:DISPLAY".into(), "DESCRIPTION:Reminder".into(), format!("TRIGGER:-PT{minutes}M"), "END:VALARM".into()]);
    }
    lines.extend(["END:VEVENT".to_string(), "END:VCALENDAR".into()]);
    lines.iter().map(|l| crate::ics::fold(l)).collect()
}

/// A calendar file's lines, unfolded.
fn unfold_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.trim_end_matches('\r');
        if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
            lines.last_mut().expect("checked").push_str(&raw[1..]);
        } else if !raw.is_empty() {
            lines.push(raw.to_string());
        }
    }
    lines
}

fn prop_name(line: &str) -> String {
    line.split([':', ';']).next().unwrap_or("").trim().to_ascii_uppercase()
}

/// The VEVENT blocks of a calendar file: (first line, END line, its RECURRENCE-ID if any).
fn event_blocks(lines: &[String]) -> Vec<(usize, usize, Option<String>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].eq_ignore_ascii_case("BEGIN:VEVENT") {
            let mut depth = 0;
            let mut rid = None;
            let mut j = i + 1;
            while j < lines.len() {
                let n = prop_name(&lines[j]);
                if n == "BEGIN" {
                    depth += 1;
                } else if n == "END" {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                } else if depth == 0 && n == "RECURRENCE-ID" {
                    rid = lines[j].split_once(':').map(|(_, v)| v.trim().to_string());
                }
                j += 1;
            }
            out.push((i, j, rid));
            i = j;
        }
        i += 1;
    }
    out
}

/// Sets properties in the VEVENT at lines `begin..end` (only its own lines, not an alarm's
/// inside it): those named are taken out, the new ones put in before the first part inside it
/// or its END. Returns how many lines it now has more or fewer.
fn set_in_block(lines: &mut Vec<String>, begin: usize, end: usize, set: &[(&str, Vec<String>)]) -> isize {
    let names: Vec<&str> = set.iter().map(|(n, _)| *n).collect();
    let mut depth = 0;
    let mut kept = Vec::new();
    let mut insert_at = None;
    for (k, line) in lines[begin + 1..end].iter().enumerate() {
        let n = prop_name(line);
        if n == "BEGIN" {
            if depth == 0 && insert_at.is_none() {
                insert_at = Some(k);
            }
            depth += 1;
        } else if n == "END" {
            depth -= 1;
        } else if depth == 0 && names.contains(&n.as_str()) {
            continue;
        }
        kept.push(line.clone());
    }
    // (where the first inner part was, among the lines kept)
    let mut at = kept.len();
    if insert_at.is_some() {
        let mut depth = 0;
        for (k, line) in kept.iter().enumerate() {
            if prop_name(line) == "BEGIN" && depth == 0 {
                at = k;
                break;
            }
            match prop_name(line).as_str() {
                "BEGIN" => depth += 1,
                "END" => depth -= 1,
                _ => {}
            }
        }
    }
    let new: Vec<String> = set.iter().flat_map(|(_, l)| l.clone()).collect();
    kept.splice(at..at, new);
    let before = end - begin - 1;
    let after = kept.len();
    lines.splice(begin + 1..end, kept);
    after as isize - before as isize
}

fn refold(lines: &[String]) -> String {
    lines.iter().map(|l| crate::ics::fold(l)).collect()
}

/// An event changed as Eudora's editor changed it (`patch`): the series (or the single event)
/// itself; its SEQUENCE raised and DTSTAMP renewed, so the people invited are sent the change.
pub fn update_event_ics(original: &str, p: &Value) -> Result<String> {
    let mut lines = unfold_lines(original);
    let (begin, end, _) = event_blocks(&lines).into_iter().find(|b| b.2.is_none()).ok_or_else(|| Error::Other("the event on the server has no main entry".into()))?;
    let sequence = lines[begin..end].iter().find(|l| prop_name(l) == "SEQUENCE").and_then(|l| l.split_once(':')).and_then(|(_, v)| v.trim().parse::<i64>().ok()).unwrap_or(0);
    let mut set = lines_from_patch(p);
    set.push(("SEQUENCE", vec![format!("SEQUENCE:{}", sequence + 1)]));
    set.push(("DTSTAMP", vec![format!("DTSTAMP:{}", utc_stamp(crate::library::now()))]));
    set_in_block(&mut lines, begin, end, &set);
    Ok(refold(&lines))
}

/// The user's answer to an invitation (accept, tentative, decline) in every entry of the event
/// (the series and its changed occurrences): their ATTENDEE line's PARTSTAT.
pub fn respond_ics(original: &str, me: &str, answer: &str) -> Result<String> {
    let partstat = match answer {
        "accept" => "ACCEPTED",
        "tentative" | "tentativelyAccept" => "TENTATIVE",
        "decline" => "DECLINED",
        other => return Err(Error::Other(format!("unknown answer {other}"))),
    };
    let me = me.to_lowercase();
    let mut lines = unfold_lines(original);
    let mut found = false;
    for line in lines.iter_mut() {
        if prop_name(line) != "ATTENDEE" {
            continue;
        }
        let Some((head, value)) = line.split_once(':').map(|(h, v)| (h.to_string(), v.to_string())) else { continue };
        if !value.trim().to_lowercase().trim_start_matches("mailto:").eq(&me) {
            continue;
        }
        // (the line's head split at its parameters; quoted ones may hold ';' or ':')
        let mut params: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for c in head.chars() {
            if c == '"' {
                quoted = !quoted;
            }
            if c == ';' && !quoted {
                params.push(std::mem::take(&mut current));
            } else {
                current.push(c);
            }
        }
        params.push(current);
        let mut kept: Vec<String> = params.into_iter().filter(|p| !p.to_ascii_uppercase().starts_with("PARTSTAT=") && !p.to_ascii_uppercase().starts_with("RSVP=")).collect();
        kept.push(format!("PARTSTAT={partstat}"));
        *line = format!("{}:{value}", kept.join(";"));
        found = true;
    }
    if !found {
        return Err(Error::Other(format!("{me} isn't among the event's people on the server")));
    }
    Ok(refold(&lines))
}

/// One occurrence taken out of a series (EXDATE on the series), as deleting it in a calendar does.
pub fn without_occurrence_ics(original: &str, rid: &str) -> Result<String> {
    let mut lines = unfold_lines(original);
    let blocks = event_blocks(&lines);
    // (a changed copy of that occurrence goes too)
    if let Some((b, e, _)) = blocks.iter().rev().find(|b| b.2.as_deref() == Some(rid)).cloned() {
        lines.drain(b..=e);
    }
    let blocks = event_blocks(&lines);
    let (begin, end, _) = blocks.into_iter().find(|b| b.2.is_none()).ok_or_else(|| Error::Other("the series on the server has no main entry".into()))?;
    let exdate = if rid.len() == 8 { format!("EXDATE;VALUE=DATE:{rid}") } else { format!("EXDATE:{rid}") };
    lines.insert(end, exdate);
    let _ = begin;
    Ok(refold(&lines))
}

// ------------------------------------------------------------------ the library

impl Library {
    /// The accounts with a contacts and calendars server.
    pub fn dav_accounts(&self) -> Result<Vec<DavConfig>> {
        let mut stmt = self.conn.prepare("SELECT value FROM settings_items WHERE key LIKE ?1 || '%' ORDER BY key")?;
        let rows: Vec<String> = stmt.query_map([SETTINGS_PREFIX], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows.iter().filter_map(|j| serde_json::from_str(j).ok()).collect())
    }

    /// The library's account an address's contacts and calendars belong to (its IMAP / POP3
    /// account, else its own).
    pub fn dav_account_id(&self, address: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM accounts WHERE kind IN ('imap', 'pop3', 'dav') AND lower(address) = lower(?1) ORDER BY kind = 'dav', id LIMIT 1",
                [address],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The contacts and calendars server of an account (by the library's account id).
    pub fn dav_config_of(&self, account: i64) -> Result<Option<DavConfig>> {
        let address: Option<String> = self.conn.query_row("SELECT address FROM accounts WHERE id = ?1", params![account], |r| r.get(0)).optional()?.flatten();
        let Some(address) = address else { return Ok(None) };
        let config = self.dav_accounts()?.into_iter().find(|c| c.address.eq_ignore_ascii_case(&address));
        match config {
            Some(c) if self.dav_account_id(&c.address)? == Some(account) => Ok(Some(c)),
            _ => Ok(None),
        }
    }

    /// Adds or changes an account's contacts and calendars server (`config` as found by
    /// `discover`), the password on this computer (none: kept, or the mail password). Returns
    /// the account.
    pub fn save_dav_account(&self, config: &DavConfig, password: Option<&str>) -> Result<i64> {
        if !config.address.contains('@') {
            return Err(Error::Other("the account's email address is missing".into()));
        }
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            crate::graph::store_secret(&secret_name(&config.address), p.as_bytes())?;
        }
        let id = match self.dav_account_id(&config.address)? {
            Some(id) => id,
            None => crate::db::import_account(&self.conn, "dav", Some(&config.address), "", &config.server, false, crate::library::now())?,
        };
        self.set_setting(&format!("{SETTINGS_PREFIX}{}", config.address.to_lowercase()), &serde_json::to_string(config)?)?;
        Ok(id)
    }

    /// Finds an address's contacts and calendars server (`discover`) and saves it: `password`
    /// typed (kept on this computer), else the one kept, else the IMAP account's of that address.
    /// The network part runs without the library: call `find_dav` first, then this.
    pub fn add_dav_account(&self, config: &DavConfig, found: &Found, password: Option<&str>) -> Result<i64> {
        let mut config = config.clone();
        config.contacts = found.contacts.clone();
        config.calendars = found.calendars.clone();
        self.save_dav_account(&config, password)
    }

    /// The IMAP server of the same address, if there's an IMAP account (one of the places looked).
    pub fn dav_imap_host(&self, address: &str) -> Option<String> {
        self.imap_accounts().ok()?.into_iter().find(|c| c.address.eq_ignore_ascii_case(address)).map(|c| c.server)
    }

    /// Removes an account's contacts and calendars (here: the server keeps them).
    pub fn remove_dav_account(&self, address: &str) -> Result<()> {
        if let Some(id) = self.dav_account_id(address)? {
            self.conn.execute("DELETE FROM cards WHERE account = ?1", params![id])?;
            self.conn.execute("DELETE FROM events WHERE calendar IN (SELECT id FROM calendars WHERE account = ?1)", params![id])?;
            self.conn.execute("DELETE FROM calendars WHERE account = ?1", params![id])?;
            let own: bool = self.conn.query_row("SELECT kind = 'dav' FROM accounts WHERE id = ?1", params![id], |r| r.get(0))?;
            if own {
                self.conn.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
            }
        }
        self.conn.execute("DELETE FROM settings_items WHERE key = ?1", [format!("{SETTINGS_PREFIX}{}", address.to_lowercase())])?;
        crate::graph::delete_secret(&secret_name(address));
        Ok(())
    }

    /// An account's contacts as read from its server (books: (href, name, {id, etag, vcard})).
    pub fn record_dav_contacts(&self, account: i64, books: &[(Option<String>, String, Vec<Value>)]) -> Result<crate::cards::CardsRecorded> {
        self.record_cards_from(account, books, crate::cards::Source::Dav)
    }
}

// ------------------------------------------------------------------ the change round

/// A contact change for a CardDAV server.
#[derive(Clone, Debug)]
pub struct DavCard {
    pub op: i64,
    pub card: Option<i64>,
    pub change: DavCardChange,
}

#[derive(Clone, Debug)]
pub enum DavCardChange {
    /// In this address book ("" = the first one).
    Create { book: String, card: crate::cards::Card },
    Update { href: String, etag: Option<String>, card: crate::cards::Card, original: Option<String> },
    Delete { href: String },
}

/// An event change for a CalDAV server.
#[derive(Clone, Debug)]
pub struct DavEvent {
    pub op: i64,
    pub event: Option<i64>,
    pub calendar: String,
    pub server_id: String,
    /// create, update, respond, cancel, delete
    pub action: String,
    pub answer: Option<String>,
    pub patch: Option<Value>,
}

/// One account's work this round.
#[derive(Clone, Debug)]
pub struct DavWork {
    pub account: i64,
    pub config: DavConfig,
    /// The user's name (organizer of events made here).
    pub name: String,
    pub contacts: bool,
    pub calendars: Option<(i64, i64)>,
    pub cards: Vec<DavCard>,
    pub events: Vec<DavEvent>,
}

impl DavWork {
    pub fn is_empty(&self) -> bool {
        !self.contacts && self.calendars.is_none() && self.cards.is_empty() && self.events.is_empty()
    }
}

/// The next round: each account's contacts and calendars when they're due, and the changes
/// made here waiting for its server (up to `limit` of each).
pub fn plan(library: &Library, limit: usize) -> Result<Vec<DavWork>> {
    let conn = &library.conn;
    let mut work: HashMap<i64, DavWork> = HashMap::new();
    let mut by_account: HashMap<i64, DavConfig> = HashMap::new();
    for config in library.dav_accounts()? {
        if let Some(id) = library.dav_account_id(&config.address)? {
            by_account.insert(id, config);
        }
    }
    if by_account.is_empty() {
        return Ok(Vec::new());
    }
    let name_of = |account: i64| -> String { conn.query_row("SELECT COALESCE(name, '') FROM accounts WHERE id = ?1", params![account], |r| r.get(0)).unwrap_or_default() };
    let new = |account: i64, config: &DavConfig| DavWork { account, config: config.clone(), name: name_of(account), contacts: false, calendars: None, cards: Vec::new(), events: Vec::new() };
    for (account, config) in &by_account {
        let a = config.address.to_lowercase();
        let contacts = config.contacts.is_some() && crate::gmail_changes::due_and_noted(&format!("dav-contacts:{a}"), 3600);
        let calendars = if config.calendars.is_some() { crate::gmail_changes::calendars_due(&format!("dav:{a}"), 15) } else { None };
        if contacts || calendars.is_some() {
            let w = work.entry(*account).or_insert_with(|| new(*account, config));
            w.contacts = contacts;
            w.calendars = calendars;
        }
    }
    // contacts made, changed or deleted here
    let ops: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, kind, payload FROM pending_ops WHERE kind IN ('card-create', 'card-update', 'card-delete') AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![crate::server_changes::GIVE_UP, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (op, kind, payload) in ops {
        let v: Value = serde_json::from_str(&payload)?;
        let card = v["card"].as_i64();
        let row: Option<(i64, String, Option<String>, Option<String>, String, Option<String>)> = match card {
            Some(c) => conn
                .query_row("SELECT account, book, server_id, version, card, original FROM cards WHERE id = ?1", params![c], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                .optional()?,
            None => None,
        };
        let Some(account) = row.as_ref().map(|r| r.0).or_else(|| v["account"].as_i64()) else { continue };
        let Some(config) = by_account.get(&account) else { continue }; // (another kind of account's)
        let change = match (kind.as_str(), row) {
            ("card-delete", _) => match v["server_id"].as_str() {
                Some(href) => DavCardChange::Delete { href: href.to_string() },
                None => continue,
            },
            ("card-create", Some((_, book, _, _, json, _))) => DavCardChange::Create { book, card: serde_json::from_str(&json)? },
            ("card-update", Some((_, _, Some(href), etag, json, original))) => {
                let original = original.and_then(|o| serde_json::from_str::<Value>(&o).ok()).and_then(|o| o["vcard"].as_str().map(str::to_string));
                DavCardChange::Update { href, etag, card: serde_json::from_str(&json)?, original }
            }
            _ => {
                conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?; // (goes with its creation)
                continue;
            }
        };
        let w = work.entry(account).or_insert_with(|| new(account, config));
        if w.cards.len() < limit {
            w.cards.push(DavCard { op, card, change });
        }
    }
    // events made, changed, answered or deleted here
    let ops: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, payload FROM pending_ops WHERE kind = 'event-change' AND attempts < ?1 ORDER BY id LIMIT ?2")?;
        stmt.query_map(params![crate::server_changes::GIVE_UP, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (op, payload) in ops {
        let v: Value = serde_json::from_str(&payload)?;
        let (Some(account), Some(server_id)) = (v["account"].as_i64(), v["server_id"].as_str()) else { continue };
        let Some(config) = by_account.get(&account) else { continue };
        let action = v["action"].as_str().unwrap_or("").to_string();
        // (tickets are attached on Microsoft calendars only)
        if action == "attach" {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![op])?;
            continue;
        }
        if action != "create" && server_id.starts_with("local:") {
            continue; // (goes once the event is made there)
        }
        let w = work.entry(account).or_insert_with(|| new(account, config));
        if w.events.len() < limit {
            w.events.push(DavEvent {
                op,
                event: v["event"].as_i64(),
                calendar: v["calendar"].as_str().unwrap_or("").to_string(),
                server_id: server_id.to_string(),
                action,
                answer: v["answer"].as_str().map(str::to_string),
                patch: v.get("patch").cloned(),
            });
        }
    }
    let mut out: Vec<DavWork> = work.into_values().filter(|w| !w.is_empty()).collect();
    out.sort_by(|a, b| a.config.address.cmp(&b.config.address));
    Ok(out)
}

/// A random id for a new contact or event (its UID, and its file's name on the server).
fn new_uid() -> String {
    let bytes = crate::graph::random(16).unwrap_or_else(|_| crate::library::now().to_le_bytes().repeat(2));
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]).to_uppercase()
}

/// Whether an error says the thing is no longer on the server.
/// Whether anything answers at an address's host and port within 4 seconds (a port a firewall
/// drops would otherwise hold discovery for the client's 20-second connect timeout).
fn port_open(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Some(host_port) = url.split("://").nth(1).and_then(|r| r.split('/').next()) else { return false };
    let Ok(addresses) = host_port.to_socket_addrs() else { return false };
    addresses.into_iter().any(|a| std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_secs(4)).is_ok())
}

/// Two ETags name the same version (quotes and a weak mark aside: servers write them differently).
fn same_etag(a: &str, b: &str) -> bool {
    let bare = |s: &str| s.trim().trim_start_matches("W/").trim_matches('"').to_string();
    bare(a) == bare(b)
}

fn gone(e: &Error) -> bool {
    let t = e.to_string();
    t.contains("answered 404") || t.contains("answered 410")
}

/// A change's failure as the queue keeps it ("canceled": no try counted, the network is down).
fn failed(e: Error) -> String {
    if matches!(e, Error::Network(_)) { "canceled".to_string() } else { e.to_string() }
}

/// Makes the changes on the servers and reads what's due (the change thread: no database here).
pub fn carry_out(work: &[DavWork]) -> crate::server_changes::BatchResult {
    let mut result = crate::server_changes::BatchResult::default();
    for w in work {
        let Some(password) = password(&w.config.address) else {
            let why = format!("no password for {}'s contacts and calendars on this computer: enter it in Settings ▸ Accounts", w.config.address);
            result.cards_done.extend(w.cards.iter().map(|c| crate::cards::CardDone { op: c.op, card: c.card, result: Err(why.clone()) }));
            result.events_done.extend(w.events.iter().map(|e| (e.op, e.event, Err(why.clone()))));
            continue;
        };
        let client = DavClient::new(w.config.user(), &password);
        let me = w.config.address.to_lowercase();
        // contacts changed here
        let books = std::cell::OnceCell::new();
        let first_book = || -> Option<String> {
            books.get_or_init(|| w.config.contacts.as_deref().and_then(|home| collections(&client, home, "addressbook").ok()).unwrap_or_default()).iter().find(|b| b.writable).map(|b| b.href.clone())
        };
        let home = w.config.contacts.clone().unwrap_or_default();
        for c in &w.cards {
            let done = (|| -> Result<Option<Value>> {
                match &c.change {
                    DavCardChange::Create { book, card } => {
                        let book = if book.is_empty() { first_book().ok_or_else(|| Error::Other("the server has no address book to add contacts to".into()))? } else { resolve(&home, book) };
                        let uid = new_uid();
                        let text = vcard_for(card, None, &uid);
                        let url = format!("{}/{uid}.vcf", book.trim_end_matches('/'));
                        let a = client.checked("PUT", &url, &[("Content-Type", "text/vcard; charset=utf-8"), ("If-None-Match", "*")], Some(&text))?;
                        let etag = a.etag.unwrap_or_default();
                        Ok(Some(json!({ "id": path_of(&url), "etag": etag, "changeKey": etag, "vcard": text })))
                    }
                    DavCardChange::Update { href, etag, card, original } => {
                        let url = resolve(&home, href);
                        let text = vcard_for(card, original.as_deref(), &new_uid());
                        let mut headers = vec![("Content-Type", "text/vcard; charset=utf-8")];
                        if let Some(e) = etag.as_deref() {
                            headers.push(("If-Match", e));
                        }
                        // Some servers ignore If-Match (cPanel's): the version asked for first.
                        if let Some(e) = etag.as_deref() {
                            if client.etag_of(&url)?.is_some_and(|now| !same_etag(&now, e)) {
                                return Err(Error::Other("the contact was changed on the server since: your change waits until Eudora reads the server's copy (within the hour); then make it again".into()));
                            }
                        }
                        let a = client.send("PUT", &url, &headers, Some(&text))?;
                        if a.status == 412 {
                            return Err(Error::Other("the contact was changed on the server since: your change waits until Eudora reads the server's copy (within the hour); then make it again".into()));
                        }
                        if a.status >= 400 {
                            return Err(Error::Other(format!("PUT {url}: the server answered {}", a.status)));
                        }
                        let etag = a.etag.unwrap_or_default();
                        Ok(Some(json!({ "id": path_of(&url), "etag": etag, "changeKey": etag, "vcard": text })))
                    }
                    DavCardChange::Delete { href } => match client.checked("DELETE", &resolve(&home, href), &[], None) {
                        Err(e) if gone(&e) => Ok(None),
                        other => other.map(|_| None),
                    },
                }
            })();
            result.cards_done.push(crate::cards::CardDone { op: c.op, card: c.card, result: done.map_err(failed) });
        }
        if w.contacts && w.config.contacts.is_some() {
            let books = (|| -> Result<Vec<(Option<String>, String, Vec<Value>)>> {
                let home = w.config.contacts.as_deref().unwrap_or_default();
                collections(&client, home, "addressbook")?.into_iter().map(|b| Ok((Some(path_of(&b.href)), b.name.clone(), contacts(&client, &b.href)?))).collect()
            })();
            result.contacts.push(crate::cards::ContactsFetched { account: w.account, books: books.map_err(|e| e.to_string()), source: crate::cards::Source::Dav });
        }
        // events changed here
        let cal_home = w.config.calendars.clone().unwrap_or_default();
        for e in &w.events {
            let done = event_change(&client, &cal_home, e, &me, &w.name);
            let done = match done {
                Err(err) if gone(&err) && e.action != "create" && e.action != "update" => Ok(None),
                other => other.map_err(failed),
            };
            result.events_done.push((e.op, e.event, done));
        }
        if let Some((from, to)) = w.calendars {
            let calendars = (|| -> Result<Vec<(Value, Vec<Value>)>> {
                let list = collections(&client, &cal_home, "calendar")?;
                list.iter().enumerate().map(|(i, c)| Ok((calendar_as_graph(c, i == 0), events(&client, &c.href, from, to, &me)?))).collect()
            })();
            result.calendars.push(crate::calendar::CalendarFetched { account: w.account, from, to, calendars: calendars.map_err(|e| e.to_string()) });
        }
    }
    result
}

/// An event change on a CalDAV server; a made or changed event comes back in Graph's shape.
fn event_change(client: &DavClient, home: &str, e: &DavEvent, me: &str, my_name: &str) -> Result<Option<Value>> {
    let empty = Value::Null;
    let patch = e.patch.as_ref().unwrap_or(&empty);
    let (href, rid) = match e.server_id.split_once('#') {
        Some((h, r)) => (h.to_string(), Some(r.to_string())),
        None => (e.server_id.clone(), None),
    };
    let url = resolve(home, &href);
    let read = || -> Result<(String, Option<String>)> {
        let a = client.checked("GET", &url, &[], None)?;
        Ok((a.body, a.etag))
    };
    let write = |text: &str, etag: Option<&str>| -> Result<Option<String>> {
        let mut headers = vec![("Content-Type", "text/calendar; charset=utf-8")];
        if let Some(t) = etag {
            headers.push(("If-Match", t));
        }
        let a = client.send("PUT", &url, &headers, Some(text))?;
        if a.status == 412 {
            return Err(Error::Other("the event was changed on the server since: your change waits until Eudora reads the server's copy; then make it again".into()));
        }
        if a.status >= 400 {
            return Err(Error::Other(format!("PUT {url}: the server answered {}", a.status)));
        }
        Ok(a.etag)
    };
    let back = |text: &str, etag: Option<String>, href: &str| -> Option<Value> {
        let file = crate::ics::read(text);
        let ev = file.events.iter().find(|x| x.recurrence_id.is_none()).or(file.events.first())?;
        Some(event_as_graph(ev, href, &etag.unwrap_or_default(), me, file.events.len() > 1 || ev.repeats.is_some()))
    };
    Ok(match e.action.as_str() {
        "create" => {
            let calendar = if e.calendar.is_empty() { home.to_string() } else { resolve(home, &e.calendar) };
            let uid = new_uid();
            let text = new_event_ics(&uid, patch, me, my_name);
            let new_url = format!("{}/{uid}.ics", calendar.trim_end_matches('/'));
            let a = client.checked("PUT", &new_url, &[("Content-Type", "text/calendar; charset=utf-8"), ("If-None-Match", "*")], Some(&text))?;
            back(&text, a.etag, &path_of(&new_url))
        }
        "update" => {
            if rid.is_some() {
                return Err(Error::Other("changing one occurrence of a repeating event isn't done on this server yet: change the series, or change it in the server's own calendar".into()));
            }
            let (text, etag) = read()?;
            let changed = update_event_ics(&text, patch)?;
            let etag = write(&changed, etag.as_deref())?;
            back(&changed, etag, &href)
        }
        "respond" => {
            let (text, etag) = read()?;
            let changed = respond_ics(&text, me, e.answer.as_deref().unwrap_or(""))?;
            write(&changed, etag.as_deref())?;
            None
        }
        // (the server tells the people invited, when it schedules for its users)
        "cancel" | "delete" => {
            match rid {
                Some(rid) => {
                    let (text, etag) = read()?;
                    write(&without_occurrence_ics(&text, &rid)?, etag.as_deref())?;
                }
                None => {
                    client.checked("DELETE", &url, &[], None)?;
                }
            }
            None
        }
        other => return Err(Error::Other(format!("{other} isn't done on CalDAV servers"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etags_match_however_the_server_writes_them() {
        assert!(same_etag("\"abc\"", "abc"));
        assert!(same_etag("W/\"abc\"", "\"abc\""));
        assert!(same_etag("1794464653", "1794464653"));
        assert!(!same_etag("1794464653", "1794464742"));
    }

    #[test]
    fn a_multistatus_reads_with_any_namespace_prefixes() {
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/principals/ann/</D:href>
<D:propstat><D:prop><D:current-user-principal><D:href>/dav/principals/ann/</D:href></D:current-user-principal>
<C:addressbook-home-set><D:href>/dav/books/ann/</D:href></C:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
<D:propstat><D:prop><cal:calendar-home-set xmlns:cal="urn:ietf:params:xml:ns:caldav"/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>
</D:response></D:multistatus>"#;
        let items = multistatus(xml).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].props.child("addressbook-home-set").and_then(Node::href).as_deref(), Some("/dav/books/ann/"));
        assert!(items[0].props.child("calendar-home-set").is_none(), "a 404 property isn't there");
        // entities in text come back as the characters
        let n = parse_xml("<a><b>Tom &amp; Jerry &#x263A;</b></a>").unwrap();
        assert_eq!(n.child("b").unwrap().text, "Tom & Jerry \u{263A}");
    }

    #[test]
    fn hrefs_resolve_against_where_they_came_from() {
        assert_eq!(resolve("https://h.example/a/b/", "/c/d"), "https://h.example/c/d");
        assert_eq!(resolve("https://h.example/a/b/", "c.vcf"), "https://h.example/a/b/c.vcf");
        assert_eq!(resolve("https://h.example:2080/a/", "https://o.example/x"), "https://o.example/x");
        assert!(same_href("/a/b%20c/", "https://h.example/a/b c"));
    }

    #[test]
    fn a_vcard_keeps_what_eudora_doesnt_edit() {
        let original = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:abc-1\r\nFN:Ann Old\r\nN:Old;Ann;;;\r\nEMAIL;TYPE=work:old@example.com\r\nPHOTO;ENCODING=b;TYPE=JPEG:AAAA\r\n BBBB\r\nX-SOCIAL:ann\r\nEND:VCARD\r\n";
        let card = crate::cards::Card { given: "Ann".into(), family: "Smith".into(), emails: vec![crate::cards::Labeled { kind: "work".into(), value: "ann@example.com".into() }], ..Default::default() };
        let text = vcard_for(&card, Some(original), "ignored");
        assert!(text.contains("UID:abc-1") && !text.contains("ignored"), "{text}");
        assert!(text.contains("PHOTO;ENCODING=b;TYPE=JPEG:AAAABBBB"), "{text}");
        assert!(text.contains("X-SOCIAL:ann"));
        assert!(text.contains("ann@example.com") && !text.contains("old@example.com"));
        assert_eq!(text.matches("BEGIN:VCARD").count(), 1);
        assert!(text.trim_end().ends_with("END:VCARD"));
        let read = crate::cards::from_vcard(&text);
        assert_eq!(read[0].family, "Smith");
        // a new card gets its UID
        assert!(vcard_for(&card, None, "NEW-1").contains("UID:NEW-1"));
    }

    #[test]
    fn an_event_reads_in_graphs_shape_and_changes_keep_the_rest() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTART:20261201T150000Z\r\nDTEND:20261201T160000Z\r\nSUMMARY:Plan\r\nSEQUENCE:2\r\nORGANIZER;CN=Bob:mailto:bob@example.com\r\nATTENDEE;CN=Ann;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:ann@example.com\r\nX-KEEP:yes\r\nBEGIN:VALARM\r\nTRIGGER:-PT15M\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let file = crate::ics::read(ics);
        let g = event_as_graph(&file.events[0], "/cal/ann/u1.ics", "\"e1\"", "ann@example.com", false);
        assert_eq!(g["id"], "/cal/ann/u1.ics");
        assert_eq!(g["start"]["dateTime"], "2026-12-01T15:00:00");
        assert_eq!(g["responseStatus"]["response"], "notResponded");
        assert_eq!(g["isOrganizer"], false);
        let e = crate::calendar::from_graph(&g);
        assert_eq!(e.subject, "Plan");

        let changed = update_event_ics(ics, &json!({ "subject": "Plan B", "start": { "dateTime": "2026-12-02T15:00:00" }, "end": { "dateTime": "2026-12-02T16:00:00" } })).unwrap();
        assert!(changed.contains("SUMMARY:Plan B") && !changed.contains("SUMMARY:Plan\r"), "{changed}");
        assert!(changed.contains("DTSTART:20261202T150000Z") && !changed.contains("20261201T150000Z"));
        assert!(changed.contains("SEQUENCE:3") && changed.contains("X-KEEP:yes") && changed.contains("TRIGGER:-PT15M"));
        let at_end = changed.find("SUMMARY:Plan B").unwrap() < changed.find("BEGIN:VALARM").unwrap();
        assert!(at_end, "new lines go before the alarm, inside the event");

        let answered = respond_ics(ics, "Ann@Example.com", "accept").unwrap();
        assert!(answered.contains("PARTSTAT=ACCEPTED:mailto:ann@example.com") && !answered.contains("RSVP=TRUE"), "{answered}");
        assert!(respond_ics(ics, "zed@example.com", "accept").is_err());
    }

    #[test]
    fn an_occurrence_is_taken_out_with_an_exdate() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:s\r\nDTSTART:20261201T150000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Weekly\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:s\r\nRECURRENCE-ID:20261208T150000Z\r\nDTSTART:20261208T170000Z\r\nSUMMARY:Moved\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let out = without_occurrence_ics(ics, "20261208T150000Z").unwrap();
        assert!(out.contains("EXDATE:20261208T150000Z") && !out.contains("SUMMARY:Moved"), "{out}");
        assert_eq!(out.matches("BEGIN:VEVENT").count(), 1);
    }

    #[test]
    fn a_new_event_has_its_people_and_reminder() {
        let p = json!({ "subject": "Lunch", "start": { "dateTime": "2026-12-03T17:00:00" }, "end": { "dateTime": "2026-12-03T18:00:00" }, "isAllDay": false,
            "location": { "displayName": "Cafe" }, "body": { "content": "<p>Bring <b>notes</b></p>" }, "attendees": [{ "emailAddress": { "address": "bob@example.com", "name": "Bob" } }],
            "isReminderOn": true, "reminderMinutesBeforeStart": 10 });
        let text = new_event_ics("U-1", &p, "ann@example.com", "Ann");
        let file = crate::ics::read(&text);
        let e = &file.events[0];
        assert_eq!((e.summary.as_str(), e.location.as_str(), e.uid.as_str()), ("Lunch", "Cafe", "U-1"));
        assert_eq!(e.organizer.as_ref().unwrap().1, "ann@example.com");
        assert_eq!(e.attendees[0].1, "bob@example.com");
        assert!(e.description.contains("Bring") && !e.description.contains("<b>"));
        assert!(text.contains("TRIGGER:-PT10M"));
        // all day
        let day = new_event_ics("U-2", &json!({ "subject": "Off", "isAllDay": true, "start": { "dateTime": "2026-12-24T00:00:00" }, "end": { "dateTime": "2026-12-25T00:00:00" } }), "a@x", "A");
        assert!(day.contains("DTSTART;VALUE=DATE:20261224"));
    }
}

