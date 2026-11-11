//! Microsoft Graph: signing in to Microsoft 365 / Outlook.com and reading mail.
//!
//! Sign-in is OAuth 2.0 authorization code + PKCE with the "Eudora 27" app registration (a
//! public client: no secret). The browser shows Microsoft's sign-in and consent pages and comes
//! back to `http://localhost:<port>`, where Eudora listens just long enough to get the code.
//! The refresh token is kept in Windows Credential Manager / the macOS Keychain (never in the
//! library or settings); access tokens live in memory.
//!
//! Read-only for now (Mail.Read): nothing here changes the mailbox.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// The app registration Eudora signs in with (`official`: Planet Networks' "Eudora 27" in its
/// own builds; any organization or personal account).
pub const CLIENT_ID: &str = crate::official::MICROSOFT_CLIENT_ID;
pub const SCOPES: &str = "offline_access User.Read Mail.Read";
/// Also reading the Online Archive (the mailbox APIs; work and school accounts only, so they
/// are asked for separately: a personal account's sign-in would fail on them).
pub const ARCHIVE_SCOPES: &str = "offline_access User.Read Mail.Read MailboxFolder.Read MailboxItem.Read MailboxItem.Export";
/// Everything for a work or school account: the archive, mailboxes shared with the user, and
/// sending as the user or as a shared mailbox (Send As / Send on Behalf, as Exchange allows).
pub const ALL_SCOPES: &str = "offline_access User.Read User.ReadBasic.All Mail.Read MailboxFolder.Read MailboxItem.Read MailboxItem.Export \
     Mail.Read.Shared Mail.Send Mail.Send.Shared";
/// `ALL_SCOPES` plus changing mail on the server: moving messages and making folders, in the
/// user's own mailbox and the shared ones (added 2026-10-14; agreed at the next sign-in). A
/// sign-in from before has no consent for these: `resume_best` falls back, and the changes wait.
pub const WRITE_SCOPES: &str = "offline_access User.Read User.ReadBasic.All Mail.Read MailboxFolder.Read MailboxItem.Read MailboxItem.Export      Mail.Read.Shared Mail.Send Mail.Send.Shared Mail.ReadWrite Mail.ReadWrite.Shared";
/// What a sign-in asks for: `WRITE_SCOPES` plus Outlook's category list (labels with their
/// colors; added 2026-10-15). Only asked at sign-in, never when a sign-in is renewed: renewing
/// with a permission not agreed to yet would fail and stop the moves until the next sign-in.
/// Once agreed, Microsoft puts it in every token for Graph.
/// The last three (2026-10-23): reorganizing the Online Archive on the server (an item imported
/// into its new folder, then the original soft-deleted; archive folders made).
pub const SIGN_IN_SCOPES: &str = "offline_access User.Read User.ReadBasic.All Mail.Read MailboxFolder.Read MailboxItem.Read MailboxItem.Export Mail.Read.Shared Mail.Send Mail.Send.Shared Mail.ReadWrite Mail.ReadWrite.Shared MailboxSettings.ReadWrite Contacts.ReadWrite Contacts.ReadWrite.Shared Calendars.ReadWrite Calendars.ReadWrite.Shared Tasks.ReadWrite MailboxItem.ImportExport MailboxItem.ReadWrite MailboxFolder.ReadWrite";
/// A personal Microsoft account (Outlook.com, Hotmail, Live): reading, sending, changing mail
/// and the category list; it has no Online Archive or shared mailboxes, whose permissions it
/// would refuse.
pub const PERSONAL_SCOPES: &str = "offline_access User.Read Mail.Read Mail.Send Mail.ReadWrite MailboxSettings.ReadWrite Contacts.ReadWrite Calendars.ReadWrite Tasks.ReadWrite";
/// Renewing a personal account's sign-in: what every such sign-in agreed to (permissions added
/// later are asked for only at sign-in, then Microsoft puts them in every token).
pub const PERSONAL_RENEW_SCOPES: &str = "offline_access User.Read Mail.Read Mail.Send Mail.ReadWrite MailboxSettings.ReadWrite";
pub const GRAPH_BETA: &str = "https://graph.microsoft.com/beta";
/// The hidden folder in each mailbox where Eudora keeps its sync envelopes (never downloaded
/// as mail).
pub const SYNC_FOLDER: &str = "Eudora Sync";
/// The subject every sync envelope's message starts with.
pub const SYNC_SUBJECT: &str = "Eudora sync";
/// Largest part of a sync envelope stored in one message (a Graph request carries 4 MB, base64
/// inflates by 4/3, and the message around it needs a little): bigger envelopes go in parts.
pub const SYNC_ITEM_LIMIT: usize = 2_800 * 1024;
const AUTHORITY: &str = "https://login.microsoftonline.com/common/oauth2/v2.0";
pub const GRAPH: &str = "https://graph.microsoft.com/v1.0";
const KEYRING_SERVICE: &str = "Eudora 27";
const DEFAULT_ACCOUNT: &str = "graph:default";
/// Where Microsoft's page comes back to on iOS (a "Mobile and desktop applications" redirect on
/// the Eudora 27 app registration; the scheme is the app's bundle id).
#[cfg(target_os = "ios")]
const IOS_REDIRECT: &str = "net.planet.eudora27://auth";
#[cfg(target_os = "ios")]
const IOS_SCHEME: &str = "net.planet.eudora27";
/// How long the browser sign-in may take.
const SIGN_IN_WAIT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Clone)]
struct Tokens {
    access: String,
    refresh: String,
    expires: Instant,
}

/// One request of a JSON batch (`GraphClient::batch`).
pub struct BatchItem {
    pub method: &'static str,
    /// The whole URL, as for a request of its own.
    pub url: String,
    pub body: Option<serde_json::Value>,
}

/// What Microsoft said about one request of a batch, as an error's text.
pub fn batch_error(status: u16, body: &serde_json::Value) -> String {
    format!("Microsoft Graph {status} {}: {}", body["error"]["code"].as_str().unwrap_or(""), body["error"]["message"].as_str().unwrap_or(""))
}

/// A signed-in Microsoft account.
pub struct GraphClient {
    agent: ureq::Agent,
    tokens: Tokens,
    scopes: &'static str,
    /// The account's address (mail, else user principal name).
    pub account: String,
    pub display_name: String,
    /// Shared with the workers made from this client.
    pub link: Arc<Link>,
    /// Another mailbox the user may open (a shared mailbox, by address); None = the user's own.
    pub mailbox: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailFolder {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub parent_folder_id: Option<String>,
    #[serde(default)]
    pub child_folder_count: u64,
    #[serde(default)]
    pub total_item_count: u64,
    #[serde(default)]
    pub unread_item_count: u64,
    #[serde(default)]
    pub is_hidden: bool,
    /// Depth below the mailbox's top (filled in by `folders`).
    #[serde(skip)]
    pub depth: usize,
}

/// A folder of a mailbox seen through the mailbox APIs (also the Online Archive).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveFolder {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub child_folder_count: u64,
    #[serde(default)]
    pub total_item_count: u64,
    #[serde(default)]
    pub is_hidden: bool,
    /// Where the folder really is (auto-expanding archives span several mailboxes).
    #[serde(default)]
    pub parent_mailbox_url: Option<String>,
    #[serde(default)]
    pub well_known_name: Option<String>,
    /// Bytes in the folder (PidTagMessageSizeExtended), when Exchange gives it.
    #[serde(skip)]
    pub size: Option<u64>,
    #[serde(skip)]
    pub depth: usize,
}

/// An item of a mailbox folder (mailbox APIs).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxItem {
    pub id: String,
    /// The message class ("IPM.Note" for mail).
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub created_date_time: Option<String>,
}

/// One exported item: Exchange's opaque Fast Transfer Stream, or why it failed.
pub struct ExportedItem {
    pub item_id: String,
    pub data: Option<Vec<u8>>,
    pub error: Option<String>,
}

/// PidTagMessageSizeExtended: a folder's total size in bytes.
const SIZE_PROPERTY: &str = "Long 0x0E08";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    pub id: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub received_date_time: Option<String>,
    #[serde(default)]
    pub from: Option<serde_json::Value>,
}

impl MessageSummary {
    pub fn from_display(&self) -> String {
        let a = self.from.as_ref().and_then(|f| f.get("emailAddress"));
        let name = a.and_then(|a| a.get("name")).and_then(|v| v.as_str()).unwrap_or("");
        let addr = a.and_then(|a| a.get("address")).and_then(|v| v.as_str()).unwrap_or("");
        match (name.is_empty(), addr.is_empty()) {
            (false, false) if name != addr => format!("{name} <{addr}>"),
            (false, _) => name.to_string(),
            _ => addr.to_string(),
        }
    }
}

pub(crate) fn agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(300)))
        // a dead Wi-Fi shows up as a connection that never opens or an answer that never comes
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(90)))
        .user_agent(concat!("Eudora/27 (eudora-core ", env!("CARGO_PKG_VERSION"), ")"))
        .tls_config(tls)
        .build();
    ureq::Agent::new_with_config(config)
}

/// A failed request: no answer at all (network down, Wi-Fi changed, timeout, broken TLS) is
/// `Error::Network`, worth waiting out; anything else is a real error.
pub(crate) fn net(e: ureq::Error) -> Error {
    use ureq::Error as U;
    match e {
        U::Io(_) | U::Timeout(_) | U::HostNotFound | U::ConnectionFailed | U::Protocol(_) | U::Tls(_) | U::Rustls(_) | U::BodyStalled
        | U::Decompress(..) | U::ConnectProxyFailed(_) => Error::Network(e.to_string()),
        _ => Error::Other(format!("network: {e}")),
    }
}

/// The connection's state, shared by an account's clients (the workers too): when one finds
/// the network gone, all wait, and the download shows it.
#[derive(Default)]
pub struct Link {
    /// When the connection was lost, and why (None: working).
    lost: Mutex<Option<(Instant, String)>>,
    /// How many times it was lost and came back (for the report).
    pub outages: AtomicU64,
    stop: AtomicBool,
}

impl Link {
    /// Since when, and why, there's no connection (None: working).
    pub fn lost(&self) -> Option<(Duration, String)> {
        self.lost.lock().unwrap().as_ref().map(|(at, why)| (at.elapsed(), why.clone()))
    }

    /// Makes clients waiting for the network give up (canceled).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Ready again after `stop`: the next pass of a download that keeps watching for new mail
    /// uses the same link (without this, every check after the first pass was "canceled" at once:
    /// found 2026-10-14, when the user's first full pass finished and new mail stopped coming).
    pub fn resume(&self) {
        self.stop.store(false, Ordering::SeqCst);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn went_down(&self, why: &str) {
        let mut lost = self.lost.lock().unwrap();
        if lost.is_none() {
            *lost = Some((Instant::now(), why.to_string()));
            self.outages.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn came_back(&self) {
        let mut lost = self.lost.lock().unwrap();
        if lost.is_some() {
            *lost = None;
        }
    }
}

/// Longest wait between tries while the network is down.
const RETRY_MAX: u64 = 30;
/// Throttling (429 / 503 / 504) answers waited out for one request before giving up.
const THROTTLE_TRIES: u32 = 40;

pub(crate) fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn random(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    Ok(buf)
}

/// The Microsoft accounts signed in on this computer (one address a line), in the credential
/// store beside their sign-ins.
const ACCOUNT_LIST: &str = "graph:accounts";

fn account_list() -> Vec<String> {
    keyring_entry(ACCOUNT_LIST)
        .ok()
        .and_then(|e| e.get_password().ok())
        .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

fn set_account_list(list: &[String]) -> Result<()> {
    let entry = keyring_entry(ACCOUNT_LIST)?;
    if list.is_empty() {
        let _ = entry.delete_credential();
        return Ok(());
    }
    entry.set_password(&list.join("\n")).map_err(|e| Error::Other(format!("credential store: {e}")))
}

pub(crate) fn keyring_entry(name: &str) -> Result<keyring::Entry> {
    #[cfg(target_os = "ios")]
    {
        // keyring's own setup knows no store on iOS: the protected Keychain, set up once
        static STORE: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
        STORE
            .get_or_init(|| apple_native_keyring_store::protected::Store::new().map(|store| keyring_core::set_default_store(store)).map_err(|e| e.to_string()))
            .clone()
            .map_err(|e| Error::Other(format!("credential store: {e}")))?;
        return keyring_core::Entry::new(KEYRING_SERVICE, name).map(|inner| keyring::Entry { inner }).map_err(|e| Error::Other(format!("credential store: {e}")));
    }
    #[cfg(target_os = "android")]
    {
        // keyring knows no store on Android either: Eudora's own, in the app's private storage
        // (android_secrets.rs), set up once the app has said where that is
        static STORE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        let dir = crate::library::app_data_dir().ok_or_else(|| Error::Other("credential store: the app's storage isn't known yet".into()))?;
        STORE.get_or_init(|| keyring_core::set_default_store(crate::android_secrets::Store::new(dir.join("secrets"))));
        return keyring_core::Entry::new(KEYRING_SERVICE, name).map(|inner| keyring::Entry { inner }).map_err(|e| Error::Other(format!("credential store: {e}")));
    }
    #[allow(unreachable_code)]
    keyring::Entry::new(KEYRING_SERVICE, name).map_err(|e| Error::Other(format!("credential store: {e}")))
}

/// Windows Credential Manager holds at most 2,560 bytes per entry, and Microsoft's refresh
/// tokens are often longer as UTF-16 (a 1,653-character token failed, 2026-10-08). So a secret
/// is kept as raw bytes in pieces: `<name>#0`, `<name>#1`, …
const PIECE: usize = 2000;

pub(crate) fn store_secret(name: &str, secret: &[u8]) -> Result<()> {
    let pieces: Vec<&[u8]> = secret.chunks(PIECE).collect();
    for (i, piece) in pieces.iter().enumerate() {
        keyring_entry(&format!("{name}#{i}"))?
            .set_secret(piece)
            .map_err(|e| Error::Other(format!("couldn't keep the sign-in in the credential store: {e}")))?;
    }
    // drop pieces left over from a longer earlier secret
    let mut i = pieces.len();
    while keyring_entry(&format!("{name}#{i}"))?.delete_credential().is_ok() {
        i += 1;
    }
    Ok(())
}

pub(crate) fn load_secret(name: &str) -> Option<Vec<u8>> {
    let mut secret = Vec::new();
    for i in 0.. {
        match keyring_entry(&format!("{name}#{i}")).ok()?.get_secret() {
            Ok(piece) => secret.extend(piece),
            Err(_) if i > 0 => break,
            Err(_) => return None,
        }
    }
    Some(secret)
}

pub(crate) fn delete_secret(name: &str) {
    let mut i = 0;
    while keyring_entry(&format!("{name}#{i}")).map(|e| e.delete_credential().is_ok()).unwrap_or(false) {
        i += 1;
    }
}

/// How a phone app opens a link (Android's intent, iOS's own call): set once by the app, which
/// has the system's handle (Tauri's opener plugin); desktops use the system's command below.
type Opener = Box<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>;
static OPENER: std::sync::OnceLock<Opener> = std::sync::OnceLock::new();

pub fn set_browser_opener(opener: Opener) {
    let _ = OPENER.set(opener);
}

/// Opens a web page in the user's browser.
pub fn open_browser(url: &str) -> std::io::Result<()> {
    if let Some(open) = OPENER.get() {
        return open(url);
    }
    #[cfg(windows)]
    {
        std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn().map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn().map(|_| ())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn().map(|_| ())
    }
}

fn token_request(agent: &ureq::Agent, form: &[(&str, &str)]) -> Result<Tokens> {
    let mut response = agent.post(format!("{AUTHORITY}/token")).send_form(form.iter().copied()).map_err(net)?;
    let body: TokenResponse = response.body_mut().read_json().map_err(net)?;
    match (body.access_token, body.refresh_token) {
        (Some(access), Some(refresh)) => Ok(Tokens {
            access,
            refresh,
            expires: Instant::now() + Duration::from_secs(body.expires_in.unwrap_or(3600).saturating_sub(120)),
        }),
        _ => Err(Error::Other(format!(
            "Microsoft refused the sign-in: {} {}",
            body.error.unwrap_or_default(),
            body.error_description.unwrap_or_default().lines().next().unwrap_or("")
        ))),
    }
}

/// Waits for the browser to come back to `listener` with the authorization code.
pub(crate) fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    listener.set_nonblocking(true).map_err(|e| Error::Other(e.to_string()))?;
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).ok();
                stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).ok();
                let target = line.split_whitespace().nth(1).unwrap_or("/");
                let url = url::Url::parse(&format!("http://localhost{target}")).map_err(|e| Error::Other(e.to_string()))?;
                let get = |k: &str| url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
                let (code, error) = (get("code"), get("error"));
                if code.is_none() && error.is_none() {
                    // (e.g. the browser asking for /favicon.ico)
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let ok = code.is_some() && get("state").as_deref() == Some(state);
                let page = if ok {
                    "<h2>Signed in to Eudora.</h2><p>You can close this tab and go back to Eudora.</p>"
                } else {
                    "<h2>Eudora wasn't signed in.</h2><p>Go back to Eudora for the details.</p>"
                };
                let html = format!("<!doctype html><meta charset=utf-8><title>Eudora</title><body style=\"font-family:sans-serif;margin:3em\">{page}</body>");
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len());
                if let Some(e) = error {
                    return Err(Error::Other(format!("sign-in canceled or refused: {e} {}", get("error_description").unwrap_or_default())));
                }
                if !ok {
                    return Err(Error::Other("the sign-in reply didn't match this request (state)".into()));
                }
                return Ok(code.unwrap());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() > SIGN_IN_WAIT {
                    return Err(Error::Other("the sign-in wasn't finished within 5 minutes".into()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(Error::Other(format!("waiting for the sign-in: {e}"))),
        }
    }
}

impl GraphClient {
    /// Signs in through the browser (Microsoft's sign-in and consent pages) and remembers the
    /// account. `show(url)` opens the page (and may print it, in case no browser opens).
    pub fn sign_in(show: &dyn Fn(&str), scopes: &'static str) -> Result<GraphClient> {
        GraphClient::sign_in_as(show, scopes, None)
    }

    /// `sign_in` for one account (`hint`: its address): Microsoft's page goes to that account
    /// (`login_hint`; a personal address straight to the personal sign-in, `domain_hint`) instead
    /// of offering whichever account the browser is already signed in to.
    pub fn sign_in_as(show: &dyn Fn(&str), scopes: &'static str, hint: Option<&str>) -> Result<GraphClient> {
        if CLIENT_ID.is_empty() {
            return Err(Error::Other(
                "This build of Eudora has no Microsoft app registration, so it can't sign in to Microsoft 365 or Outlook.com. Whoever built it sets one: see BUILDING.md (EUDORA_MICROSOFT_CLIENT_ID).".into(),
            ));
        }
        let agent = agent();
        let verifier = b64url(&random(32)?);
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        let state = b64url(&random(16)?);
        // iOS: Apple's sign-in sheet, back to the app's own address (registered with Microsoft)
        #[cfg(target_os = "ios")]
        let redirect = IOS_REDIRECT.to_string();
        #[cfg(not(target_os = "ios"))]
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| Error::Other(format!("can't listen for the sign-in: {e}")))?;
        #[cfg(not(target_os = "ios"))]
        let redirect = format!("http://localhost:{}", listener.local_addr().map_err(|e| Error::Other(e.to_string()))?.port());
        let mut url = url::Url::parse(&format!("{AUTHORITY}/authorize")).unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect)
            .append_pair("response_mode", "query")
            .append_pair("scope", scopes)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        match hint.map(str::trim).filter(|h| h.contains('@')) {
            Some(address) => {
                url.query_pairs_mut().append_pair("login_hint", address);
                if crate::mail_host::is_microsoft_personal(address) {
                    url.query_pairs_mut().append_pair("domain_hint", "consumers");
                }
            }
            None => {
                url.query_pairs_mut().append_pair("prompt", "select_account");
            }
        }
        #[cfg(target_os = "ios")]
        let code = {
            let _ = show;
            crate::web_auth::code_in(&crate::web_auth::authenticate(url.as_str(), IOS_SCHEME)?, &state)?
        };
        #[cfg(not(target_os = "ios"))]
        let code = {
            show(url.as_str());
            wait_for_code(&listener, &state)?
        };
        let tokens = token_request(
            &agent,
            &[
                ("client_id", CLIENT_ID),
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &redirect),
                ("code_verifier", &verifier),
                ("scope", scopes),
            ],
        )?;
        let mut client = GraphClient { agent, tokens, scopes, account: String::new(), display_name: String::new(), link: Arc::default(), mailbox: None };
        client.load_profile()?;
        client.remember()?;
        Ok(client)
    }

    /// The account signed in last on this computer, from its stored refresh token. `scopes`
    /// must have been agreed to at sign-in (`ARCHIVE_SCOPES` after `login --archive`).
    pub fn resume(account: Option<&str>, scopes: &'static str) -> Result<GraphClient> {
        let account = match account {
            Some(a) => a.to_string(),
            None => keyring_entry(DEFAULT_ACCOUNT)?
                .get_password()
                .map_err(|_| Error::Other("no Microsoft account is signed in on this computer yet: run `eudora graph login`".into()))?,
        };
        let refresh = load_secret(&format!("graph:{account}"))
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| Error::Other(format!("{account} isn't signed in on this computer: run `eudora graph login`")))?;
        let agent = agent();
        let tokens = token_request(&agent, &[("client_id", CLIENT_ID), ("grant_type", "refresh_token"), ("refresh_token", &refresh), ("scope", scopes)])
            .map_err(|e| if scopes != SCOPES {
                Error::Other(format!("{e}\nThe archive, shared mailboxes and sending need one more sign-in: run `eudora graph login --archive`"))
            } else { e })?;
        let mut client = GraphClient { agent, tokens, scopes, account, display_name: String::new(), link: Arc::default(), mailbox: None };
        client.load_profile()?;
        client.remember()?; // (refresh tokens rotate: keep the new one)
        Ok(client)
    }

    /// The signed-in account with the most it may do: changing mail on the server
    /// (`WRITE_SCOPES`) when the user agreed to that at sign-in, else `fallback`.
    pub fn resume_best(account: Option<&str>, fallback: &'static str) -> Result<GraphClient> {
        GraphClient::resume(account, WRITE_SCOPES)
            .or_else(|_| GraphClient::resume(account, PERSONAL_RENEW_SCOPES))
            .or_else(|_| GraphClient::resume(account, fallback))
    }

    /// Signs in with everything Eudora uses (`SIGN_IN_SCOPES`); when Microsoft refuses that
    /// list (a personal Outlook.com / Hotmail account has no archive or shared mailboxes), once
    /// more with what a personal account allows (`PERSONAL_SCOPES`). `show` opens the browser.
    ///
    /// `hint`: the account meant (an address): Microsoft goes to it, a personal address asks only
    /// for what personal accounts allow, and when Microsoft signs in someone else (the browser's
    /// own account picked) it's an error saying so (and that other sign-in isn't kept unless it
    /// was here before).
    pub fn sign_in_any(show: &dyn Fn(&str), hint: Option<&str>) -> Result<GraphClient> {
        let before = GraphClient::signed_in_accounts();
        let personal = hint.is_some_and(crate::mail_host::is_microsoft_personal);
        let client = if personal {
            GraphClient::sign_in_as(show, PERSONAL_SCOPES, hint)?
        } else {
            match GraphClient::sign_in_as(show, SIGN_IN_SCOPES, hint) {
                Err(Error::Other(e)) if e.to_ascii_lowercase().contains("scope") => GraphClient::sign_in_as(show, PERSONAL_SCOPES, hint)?,
                other => other?,
            }
        };
        if let Some(wanted) = hint.map(str::trim).filter(|h| h.contains('@')) {
            if !client.account.eq_ignore_ascii_case(wanted) {
                if !before.iter().any(|a| a.eq_ignore_ascii_case(&client.account)) {
                    let _ = GraphClient::sign_out(&client.account);
                }
                return Err(Error::Other(format!(
                    "Microsoft signed in {} instead of {wanted}. Try again and, on Microsoft's page, choose \"Use another account\" (or sign out of {} in the browser first).",
                    client.account, client.account
                )));
            }
        }
        Ok(client)
    }

    /// Whether this sign-in may read the Online Archive (work and school accounts that agreed).
    pub fn may_read_archive(&self) -> bool {
        self.scopes.split(' ').any(|s| s == "MailboxItem.Export")
    }

    /// Whether this sign-in may move messages and make folders on the server.
    pub fn may_change_mail(&self) -> bool {
        self.scopes.split(' ').any(|s| s == "Mail.ReadWrite")
    }

    /// Forgets the account's stored sign-in on this computer. When it was the default, the next
    /// signed-in account becomes the default.
    pub fn sign_out(account: &str) -> Result<()> {
        delete_secret(&format!("graph:{account}"));
        let rest: Vec<String> = account_list().into_iter().filter(|a| !a.eq_ignore_ascii_case(account)).collect();
        set_account_list(&rest)?;
        if keyring_entry(DEFAULT_ACCOUNT)?.get_password().ok().is_some_and(|d| d.eq_ignore_ascii_case(account)) {
            let _ = keyring_entry(DEFAULT_ACCOUNT)?.delete_credential();
            if let Some(next) = rest.iter().find(|a| load_secret(&format!("graph:{a}")).is_some()) {
                keyring_entry(DEFAULT_ACCOUNT)?.set_password(next).map_err(|e| Error::Other(format!("credential store: {e}")))?;
            }
        }
        Ok(())
    }

    /// Every Microsoft account signed in on this computer (several may be: a work account and
    /// an Outlook.com one), the default (the first signed in) first.
    pub fn signed_in_accounts() -> Vec<String> {
        let mut all: Vec<String> = Vec::new();
        if let Some(d) = keyring_entry(DEFAULT_ACCOUNT).ok().and_then(|e| e.get_password().ok()) {
            all.push(d);
        }
        for a in account_list() {
            if !all.iter().any(|x| x.eq_ignore_ascii_case(&a)) {
                all.push(a);
            }
        }
        all.retain(|a| load_secret(&format!("graph:{a}")).is_some());
        all
    }

    /// Keeps the sign-in (refresh tokens rotate: every renewal keeps the new one). Adds the
    /// account to the list; it becomes the default only when there is none (signing in to a
    /// second account never takes the first one's place).
    fn remember(&self) -> Result<()> {
        store_secret(&format!("graph:{}", self.account), self.tokens.refresh.as_bytes())?;
        let mut list = account_list();
        if !list.iter().any(|a| a.eq_ignore_ascii_case(&self.account)) {
            list.push(self.account.clone());
            set_account_list(&list)?;
        }
        let default = keyring_entry(DEFAULT_ACCOUNT)?.get_password().ok().filter(|d| load_secret(&format!("graph:{d}")).is_some());
        if default.is_none() {
            keyring_entry(DEFAULT_ACCOUNT)?.set_password(&self.account).map_err(|e| Error::Other(format!("credential store: {e}")))?;
        }
        Ok(())
    }

    fn load_profile(&mut self) -> Result<()> {
        let me = self.get_json(&format!("{GRAPH}/me?$select=displayName,mail,userPrincipalName"))?;
        self.display_name = me["displayName"].as_str().unwrap_or_default().to_string();
        self.account = me["mail"].as_str().or_else(|| me["userPrincipalName"].as_str()).unwrap_or_default().to_string();
        Ok(())
    }

    fn refresh_if_needed(&mut self) -> Result<()> {
        if Instant::now() >= self.tokens.expires {
            self.tokens = token_request(
                &self.agent,
                &[("client_id", CLIENT_ID), ("grant_type", "refresh_token"), ("refresh_token", &self.tokens.refresh), ("scope", self.scopes)],
            )?;
            if !self.account.is_empty() {
                self.remember()?;
            }
        }
        Ok(())
    }

    /// Another client for the same account, for downloading in parallel (Exchange allows 4
    /// requests at a time per mailbox). It refreshes its own access token and never touches the
    /// stored sign-in.
    pub fn worker(&self) -> GraphClient {
        GraphClient {
            agent: self.agent.clone(),
            tokens: self.tokens.clone(),
            scopes: self.scopes,
            account: String::new(),
            display_name: String::new(),
            link: self.link.clone(),
            mailbox: self.mailbox.clone(),
        }
    }

    /// The same client opening another mailbox (a shared one, by address; None: the user's own).
    pub fn set_mailbox(&mut self, mailbox: Option<String>) {
        self.mailbox = mailbox;
    }

    /// The mailbox's base URL: the user's own (`/me`) or a shared one (`/users/{address}`).
    fn me(&self) -> String {
        match &self.mailbox {
            Some(address) => format!("{GRAPH}/users/{address}"),
            None => format!("{GRAPH}/me"),
        }
    }

    /// Everyone in the user's organization directory with a mailbox address (name, address),
    /// the user excepted: the candidates for "mailboxes I can open" (Microsoft offers apps no
    /// list of those; `graph_sync::find_mailboxes` tries each).
    pub fn directory(&mut self, progress: &dyn Fn(usize)) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut next = Some(format!("{GRAPH}/users?$select=displayName,mail&$top=999"));
        while let Some(url) = next {
            let page = self.get_json(&url)?;
            for u in page["value"].as_array().into_iter().flatten() {
                if let Some(mail) = u["mail"].as_str().filter(|m| m.contains('@') && !m.eq_ignore_ascii_case(&self.account)) {
                    out.push((u["displayName"].as_str().unwrap_or("").to_string(), mail.to_string()));
                }
            }
            progress(out.len());
            next = page["@odata.nextLink"].as_str().map(str::to_string);
        }
        Ok(out)
    }

    /// People in the organization's directory whose name, first name, last name or address
    /// starts with what's typed (address completion's last resort). (name, address); needs
    /// `User.ReadBasic.All`.
    pub fn search_directory(&mut self, typed: &str, top: usize) -> Result<Vec<(String, String)>> {
        let t = typed.trim().replace('\'', "''");
        if t.chars().count() < 2 {
            return Ok(Vec::new());
        }
        let filter = format!("startswith(displayName,'{t}') or startswith(givenName,'{t}') or startswith(surname,'{t}') or startswith(mail,'{t}')");
        let url = format!("{GRAPH}/users?$select=displayName,mail&$top={}&$filter={}", top.clamp(1, 25), url::form_urlencoded::byte_serialize(filter.as_bytes()).collect::<String>());
        let page = self.get_json(&url)?;
        Ok(page["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|u| {
                let mail = u["mail"].as_str().filter(|m| m.contains('@'))?;
                Some((u["displayName"].as_str().unwrap_or("").to_string(), mail.to_string()))
            })
            .collect())
    }

    /// The mailbox's address books: the default one ("Contacts", id None) and every contact
    /// folder (child folders too), as (id, name, contacts as Graph gives them). Needs
    /// `Contacts.Read(Write)`.
    pub fn address_books(&mut self) -> Result<Vec<(Option<String>, String, Vec<serde_json::Value>)>> {
        let me = self.me();
        let mut books = vec![(None, "Contacts".to_string(), self.all(&format!("{me}/contacts?$top=200"))?)];
        let mut queue: Vec<(String, String)> = self
            .all(&format!("{me}/contactFolders?$top=100"))?
            .into_iter()
            .filter_map(|f| Some((f["id"].as_str()?.to_string(), f["displayName"].as_str().unwrap_or("Contacts").to_string())))
            .collect();
        while let Some((id, name)) = queue.pop() {
            let contacts = self.all(&format!("{me}/contactFolders/{id}/contacts?$top=200"))?;
            for child in self.all(&format!("{me}/contactFolders/{id}/childFolders?$top=100")).unwrap_or_default() {
                if let Some(cid) = child["id"].as_str() {
                    queue.push((cid.to_string(), format!("{name}/{}", child["displayName"].as_str().unwrap_or(""))));
                }
            }
            books.push((Some(id), name, contacts));
        }
        Ok(books)
    }

    /// Makes a contact (in an address book, else the default one); returns it as Graph keeps it.
    pub fn create_contact(&mut self, book: Option<&str>, contact: &serde_json::Value) -> Result<serde_json::Value> {
        let url = match book {
            Some(id) => format!("{}/contactFolders/{id}/contacts", self.me()),
            None => format!("{}/contacts", self.me()),
        };
        self.post_json(&url, contact)
    }

    /// Changes a contact; returns it as Graph keeps it (its new changeKey).
    pub fn update_contact(&mut self, id: &str, patch: &serde_json::Value) -> Result<serde_json::Value> {
        self.send_json(true, &format!("{}/contacts/{id}", self.me()), patch)
    }

    /// Deletes a contact (Exchange keeps it in Deleted Items / Recoverable Items).
    pub fn delete_contact(&mut self, id: &str) -> Result<()> {
        let url = format!("{}/contacts/{id}", self.me());
        self.delete(&url)
    }

    /// The mailbox's calendars, as Graph gives them (id, name, hexColor, canEdit,
    /// isDefaultCalendar…).
    pub fn calendars(&mut self) -> Result<Vec<serde_json::Value>> {
        self.all(&format!("{}/calendars?$top=100", self.me()))
    }

    /// One calendar's events between two times (Unix seconds), each occurrence of a recurring
    /// series on its own, times in UTC. The body is left out (read when an event is opened).
    pub fn calendar_view(&mut self, calendar: &str, from: i64, to: i64) -> Result<Vec<serde_json::Value>> {
        const SELECT: &str = "id,changeKey,iCalUId,subject,start,end,isAllDay,location,organizer,attendees,responseStatus,showAs,isReminderOn,reminderMinutesBeforeStart,categories,sensitivity,importance,isCancelled,isOnlineMeeting,onlineMeeting,type,seriesMasterId,webLink,bodyPreview,isOrganizer,responseRequested";
        // Microsoft answers at most 1,825 days a request: slices of 1,800, an event spanning two
        // slices kept once
        const SLICE: i64 = 1_800 * 86_400;
        let iso = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut start = from;
        while start < to {
            let end = (start + SLICE).min(to);
            let mut url = Some(format!("{}/calendars/{calendar}/calendarView?startDateTime={}&endDateTime={}&$select={SELECT}&$top=500", self.me(), iso(start), iso(end)));
            while let Some(u) = url {
                let page = self.get_json_with(&u, Some("outlook.timezone=\"UTC\""))?;
                for v in page["value"].as_array().into_iter().flatten() {
                    if seen.insert(v["id"].as_str().unwrap_or("").to_string()) {
                        items.push(v.clone());
                    }
                }
                url = page["@odata.nextLink"].as_str().map(str::to_string);
            }
            start = end;
        }
        Ok(items)
    }

    /// Answers an invitation: "accept", "tentativelyAccept" or "decline", telling the organizer.
    pub fn respond_event(&mut self, id: &str, answer: &str, extra: Option<&serde_json::Value>) -> Result<()> {
        let url = format!("{}/events/{id}/{answer}", self.me());
        // a message to the organizer and (tentative / decline) a time proposed instead
        let mut body = serde_json::json!({ "sendResponse": true });
        for key in ["comment", "proposedNewTime"] {
            if let Some(v) = extra.and_then(|e| e.get(key)).filter(|v| !v.is_null()) {
                body[key] = v.clone();
            }
        }
        match self.post_json(&url, &body) {
            // an organizer who asked for no replies: the answer is kept without telling them
            // (Outlook's way; Microsoft refuses one that would send a reply)
            Err(Error::Other(why)) if why.contains("hasn't requested a response") => {
                body["sendResponse"] = false.into();
                body.as_object_mut().map(|o| o.remove("comment"));
                self.post_json(&url, &body).map(|_| ())
            }
            other => other.map(|_| ()),
        }
    }

    /// Microsoft To Do: every list with its tasks (needs `Tasks.ReadWrite`).
    pub fn todo_lists(&mut self) -> Result<Vec<(serde_json::Value, Vec<serde_json::Value>)>> {
        let me = self.me();
        let lists = self.all(&format!("{me}/todo/lists"))?;
        let mut out = Vec::new();
        for list in lists {
            let id = list["id"].as_str().unwrap_or("").to_string();
            let mut tasks = self.all(&format!("{me}/todo/lists/{id}/tasks?$top=100&$expand=linkedResources"))?;
            // a flagged email's task names the message by Outlook's id for it, which changes when
            // the message moves (Eudora's sorting moves a lot): its Message-ID, which never
            // changes, is asked for once and put beside it
            if list["wellknownListName"].as_str() == Some("flaggedEmails") {
                // (open tasks only: the list keeps every finished one too, hundreds)
                for task in tasks.iter_mut().filter(|t| t["status"].as_str() != Some("completed")) {
                    let Some(external) = task["linkedResources"][0]["externalId"].as_str().map(str::to_string) else { continue };
                    if let Some(mid) = self.message_id_of(&external) {
                        task["linkedResources"][0]["internetMessageId"] = mid.into();
                    }
                }
            }
            out.push((list, tasks));
        }
        Ok(out)
    }

    /// A message's Message-ID from Outlook's id for it, asked once and remembered while the
    /// program runs (None when Microsoft doesn't know the id: the message was deleted).
    fn message_id_of(&mut self, external: &str) -> Option<String> {
        static KNOWN: std::sync::Mutex<Option<std::collections::HashMap<String, Option<String>>>> = std::sync::Mutex::new(None);
        if let Some(found) = KNOWN.lock().ok().and_then(|k| k.as_ref().and_then(|m| m.get(external).cloned())) {
            return found;
        }
        let url = format!("{}/messages/{}?$select=internetMessageId", self.me(), external.replace('/', "%2F"));
        let found = match self.get_json(&url) {
            Ok(v) => v["internetMessageId"].as_str().map(str::to_string),
            // (network trouble: asked again next time; a missing message: remembered as missing)
            Err(e) if !matches!(e, Error::Network(_)) => None,
            Err(_) => return None,
        };
        if let Ok(mut k) = KNOWN.lock() {
            k.get_or_insert_with(Default::default).insert(external.to_string(), found.clone());
        }
        found
    }

    /// The default To Do list's id ("Tasks").
    pub fn todo_default_list(&mut self) -> Result<String> {
        let me = self.me();
        let lists = self.all(&format!("{me}/todo/lists"))?;
        lists
            .iter()
            .find(|l| l["wellknownListName"].as_str() == Some("defaultList"))
            .or(lists.first())
            .and_then(|l| l["id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| Error::Other("Microsoft To Do has no list to add to".into()))
    }

    /// Makes, changes or deletes a To Do task; returns the task as Graph keeps it.
    pub fn create_task(&mut self, list: &str, task: &serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}/todo/lists/{list}/tasks", self.me());
        self.post_json(&url, task)
    }

    pub fn update_task(&mut self, list: &str, id: &str, task: &serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}/todo/lists/{list}/tasks/{id}", self.me());
        // (linked resources are made with the task, not changed by a PATCH)
        let mut patch = task.clone();
        if let Some(o) = patch.as_object_mut() {
            o.remove("linkedResources");
        }
        self.send_json(true, &url, &patch)
    }

    pub fn delete_task(&mut self, list: &str, id: &str) -> Result<()> {
        let url = format!("{}/todo/lists/{list}/tasks/{id}", self.me());
        self.delete(&url)
    }

    /// Makes an event in a calendar; returns it as Graph keeps it (its id, its changeKey).
    pub fn create_event(&mut self, calendar: &str, event: &serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}/calendars/{calendar}/events", self.me());
        self.post_json(&url, event)
    }

    /// Changes an event (only the fields given); returns it as Graph keeps it.
    pub fn update_event(&mut self, id: &str, patch: &serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}/events/{id}", self.me());
        self.send_json(true, &url, patch)
    }

    /// Attaches a file to an event (up to 3 MB: Graph's limit for one request).
    pub fn add_event_attachment(&mut self, id: &str, name: &str, content_type: &str, bytes_base64: &str) -> Result<()> {
        let url = format!("{}/events/{id}/attachments", self.me());
        self.post_json(&url, &serde_json::json!({ "@odata.type": "#microsoft.graph.fileAttachment", "name": name, "contentType": content_type, "contentBytes": bytes_base64 }))
            .map(|_| ())
    }

    /// The names of an event's attachments.
    pub fn event_attachment_names(&mut self, id: &str) -> Result<Vec<String>> {
        let v = self.get_json(&format!("{}/events/{id}/attachments?$select=name", self.me()))?;
        Ok(v["value"].as_array().into_iter().flatten().filter_map(|a| a["name"].as_str().map(str::to_string)).collect())
    }

    /// Cancels a meeting the user organized: the people invited are told.
    pub fn cancel_event(&mut self, id: &str) -> Result<()> {
        let url = format!("{}/events/{id}/cancel", self.me());
        self.post_json(&url, &serde_json::json!({ "comment": "" })).map(|_| ())
    }

    /// Deletes an event from the calendar (Exchange keeps it in Deleted Items).
    pub fn delete_event(&mut self, id: &str) -> Result<()> {
        let url = format!("{}/events/{id}", self.me());
        self.delete(&url)
    }

    /// One event whole (its body and, for a series, its recurrence), times in UTC.
    pub fn event(&mut self, id: &str) -> Result<serde_json::Value> {
        let url = format!("{}/events/{id}", self.me());
        self.get_json_with(&url, Some("outlook.timezone=\"UTC\""))
    }

    /// Moves a message to another folder of this client's mailbox (needs `Mail.ReadWrite`);
    /// returns its new id there (Exchange gives a moved message a new id).
    pub fn move_message(&mut self, id: &str, folder: &str) -> Result<String> {
        let v = self.post_json(&format!("{}/messages/{id}/move", self.me()), &serde_json::json!({ "destinationId": folder }))?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    /// One DELETE (a message), patiently like the rest. A 404 is fine: it's gone already.
    fn delete(&mut self, url: &str) -> Result<()> {
        self.patiently(|client| {
            let mut refreshed = false;
            for attempt in 0..THROTTLE_TRIES {
                client.refresh_if_needed()?;
                let mut response = client.agent.delete(url).header("Authorization", &format!("Bearer {}", client.tokens.access)).call().map_err(net)?;
                let status = response.status().as_u16();
                if status == 401 && !refreshed {
                    refreshed = true;
                    client.tokens.expires = Instant::now();
                    continue;
                }
                if matches!(status, 429 | 503 | 504) {
                    throttled(&response, attempt);
                    continue;
                }
                // (Microsoft's answer kept: an archive folder moved elsewhere names where)
                let text = response.body_mut().read_to_string().unwrap_or_default();
                if text.contains("ErrorArchiveFolderMovedPermanently") {
                    return Err(Error::Other(format!("Microsoft Graph {status} {text}")));
                }
                if (200..300).contains(&status) || status == 404 {
                    return Ok(());
                }
                return Err(Error::Other(format!("Microsoft Graph {status} deleting {url}: {}", text.chars().take(300).collect::<String>())));
            }
            Err(Error::Other(format!("Microsoft Graph kept asking to wait: {url}")))
        })
    }

    /// The mailbox's hidden sync folder (`SYNC_FOLDER`), made when `make` and missing. None:
    /// not there (and not made).
    pub fn sync_folder(&mut self, make: bool) -> Result<Option<String>> {
        let list = format!("{}/mailFolders", self.me());
        let mut url = url::Url::parse(&list).map_err(|e| Error::Other(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("$filter", &format!("displayName eq '{SYNC_FOLDER}'"))
            .append_pair("$select", "id,displayName")
            .append_pair("includeHiddenFolders", "true");
        if let Some(id) = self.get_json(url.as_str())?["value"].as_array().and_then(|v| v.first()).and_then(|f| f["id"].as_str()) {
            return Ok(Some(id.to_string()));
        }
        if !make {
            return Ok(None);
        }
        match self.post_json(&list, &serde_json::json!({ "displayName": SYNC_FOLDER, "isHidden": true })) {
            Ok(v) => Ok(v["id"].as_str().map(str::to_string)),
            Err(Error::Other(e)) if e.contains("ErrorFolderExists") => Ok(self.get_json(url.as_str())?["value"]
                .as_array()
                .and_then(|v| v.first())
                .and_then(|f| f["id"].as_str())
                .map(str::to_string)),
            Err(e) => Err(e),
        }
    }

    /// The sync envelopes in the folder: (message id, subject, the attachment's bytes).
    pub fn sync_items(&mut self, folder: &str) -> Result<Vec<(String, String, Vec<u8>)>> {
        let mut out = Vec::new();
        let messages = self.all(&format!("{}/mailFolders/{folder}/messages?$select=id,subject&$top=100", self.me()))?;
        for m in messages {
            let (Some(id), Some(subject)) = (m["id"].as_str(), m["subject"].as_str()) else { continue };
            if !subject.starts_with(SYNC_SUBJECT) {
                continue;
            }
            let attachments = self.get_json(&format!("{}/messages/{id}/attachments", self.me()))?;
            let bytes = attachments["value"]
                .as_array()
                .and_then(|v| v.iter().find_map(|a| a["contentBytes"].as_str()))
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
            if let Some(bytes) = bytes {
                out.push((id.to_string(), subject.to_string(), bytes));
            }
        }
        Ok(out)
    }

    /// Stores an envelope in the folder as a message with it attached (subject = its name).
    /// Returns the message's id.
    pub fn put_sync_item(&mut self, folder: &str, subject: &str, bytes: &[u8]) -> Result<String> {
        if bytes.len() > SYNC_ITEM_LIMIT {
            return Err(Error::Other(format!("a sync envelope of {} KB is over the {} KB a message can carry here", bytes.len() / 1024, SYNC_ITEM_LIMIT / 1024)));
        }
        let body = serde_json::json!({
            "subject": subject,
            "body": { "contentType": "text", "content": "Eudora keeps how your mail is filed here, encrypted, for your other computers. Please leave it." },
            "internetMessageHeaders": [{ "name": "X-Eudora-Settings", "value": "1" }],
            "attachments": [{
                "@odata.type": "#microsoft.graph.fileAttachment",
                "name": "eudora-sync.bin",
                "contentType": "application/octet-stream",
                "contentBytes": base64::engine::general_purpose::STANDARD.encode(bytes),
            }],
        });
        let v = self.post_json(&format!("{}/mailFolders/{folder}/messages", self.me()), &body)?;
        v["id"].as_str().map(str::to_string).ok_or_else(|| Error::Other("Microsoft 365 stored the sync envelope but gave no id".into()))
    }

    /// Deletes a message (an envelope replaced by a newer one).
    pub fn delete_message(&mut self, id: &str) -> Result<()> {
        let url = format!("{}/messages/{id}", self.me());
        self.delete(&url)
    }

    /// Whether the mailbox still has this message (by its server id).
    pub fn message_exists(&mut self, id: &str) -> Result<bool> {
        match self.get_json(&format!("{}/messages/{id}?$select=id", self.me())) {
            Ok(_) => Ok(true),
            Err(Error::Other(e)) if e.contains("404") => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Changes a message's read state, categories and importance (needs `Mail.ReadWrite`):
    /// `patch` holds only what changes ({"isRead": true, "categories": [...], "importance": "high"}).
    /// A message's change, as one request of a batch (`batch`).
    pub fn patch_item(&self, id: &str, patch: &serde_json::Value) -> BatchItem {
        BatchItem { method: "PATCH", url: format!("{}/messages/{id}?$select=id", self.me()), body: Some(patch.clone()) }
    }

    /// A message's move, as one request of a batch (`batch`): the answer's `id` is its new id.
    pub fn move_item(&self, id: &str, folder: &str) -> BatchItem {
        BatchItem { method: "POST", url: format!("{}/messages/{id}/move?$select=id", self.me()), body: Some(serde_json::json!({ "destinationId": folder })) }
    }

    /// Several requests in one round trip (Graph's JSON batching, 20 at a time): each one's
    /// status and answer, in order. The ones Exchange asks to wait for are sent again after the
    /// wait; ones still refused after that come back as 429.
    pub fn batch(&mut self, items: &[BatchItem]) -> Result<Vec<(u16, serde_json::Value)>> {
        let mut out = vec![(0u16, serde_json::Value::Null); items.len()];
        for (c, chunk) in items.chunks(20).enumerate() {
            let base = c * 20;
            let mut todo: Vec<usize> = (0..chunk.len()).collect();
            for attempt in 0..THROTTLE_TRIES {
                if todo.is_empty() {
                    break;
                }
                let requests: Vec<serde_json::Value> = todo
                    .iter()
                    .map(|&i| {
                        let item = &chunk[i];
                        let mut r = serde_json::json!({ "id": i.to_string(), "method": item.method, "url": item.url.strip_prefix(GRAPH).unwrap_or(&item.url) });
                        if let Some(body) = &item.body {
                            r["body"] = body.clone();
                            r["headers"] = serde_json::json!({ "Content-Type": "application/json" });
                        }
                        r
                    })
                    .collect();
                let answer = self.post_json(&format!("{GRAPH}/$batch"), &serde_json::json!({ "requests": requests }))?;
                let mut again = Vec::new();
                let mut wait = 0u64;
                for response in answer["responses"].as_array().map(Vec::as_slice).unwrap_or_default() {
                    let Some(i) = response["id"].as_str().and_then(|s| s.parse::<usize>().ok()).filter(|i| *i < chunk.len()) else { continue };
                    let status = response["status"].as_u64().unwrap_or(0) as u16;
                    if matches!(status, 429 | 503 | 504) {
                        again.push(i);
                        let asked = response["headers"]["Retry-After"].as_str().and_then(|s| s.parse::<u64>().ok());
                        wait = wait.max(asked.unwrap_or(2u64 << attempt.min(4)));
                        continue;
                    }
                    out[base + i] = (status, response["body"].clone());
                }
                // (one the answer left out: sent again)
                for &i in &todo {
                    if out[base + i].0 == 0 && !again.contains(&i) {
                        again.push(i);
                    }
                }
                todo = again;
                if !todo.is_empty() {
                    std::thread::sleep(Duration::from_secs(wait.clamp(1, 60)));
                }
            }
            for i in todo {
                out[base + i] = (429, serde_json::json!({ "error": { "code": "Throttled", "message": "Microsoft Graph kept asking to wait" } }));
            }
        }
        Ok(out)
    }

    pub fn update_message(&mut self, id: &str, patch: &serde_json::Value) -> Result<()> {
        self.send_json(true, &format!("{}/messages/{id}?$select=id", self.me()), patch).map(|_| ())
    }

    /// The ids of the unread messages: the Inbox's (`inbox_only`), else the whole mailbox's
    /// (every folder of the mailbox itself; the Online Archive isn't in it).
    pub fn unread_ids(&mut self, inbox_only: bool) -> Result<Vec<String>> {
        let place = if inbox_only { "mailFolders/inbox/messages" } else { "messages" };
        let url = format!("{}/{place}?$filter=isRead%20eq%20false&$select=id&$top=1000", self.me());
        Ok(self.all(&url)?.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect())
    }

    /// The ids of the messages carrying this category, in the whole mailbox.
    pub fn ids_with_category(&mut self, category: &str) -> Result<Vec<String>> {
        let filter = format!("categories/any(c:c eq '{}')", category.replace('\'', "''"));
        let mut url = url::Url::parse(&format!("{}/messages", self.me())).map_err(|e| Error::Other(e.to_string()))?;
        url.query_pairs_mut().append_pair("$filter", &filter).append_pair("$select", "id").append_pair("$top", "1000");
        Ok(self.all(url.as_str())?.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect())
    }

    /// Makes Outlook's category list hold these labels with their colors (needs
    /// `MailboxSettings.ReadWrite`; without it labels still reach messages, uncolored in
    /// Outlook). Categories already there keep their own colors.
    pub fn ensure_categories(&mut self, labels: &[(String, String)]) -> Result<u64> {
        let url = format!("{}/outlook/masterCategories", self.me());
        let have: Vec<String> = self.all(&url)?.iter().filter_map(|c| c["displayName"].as_str().map(str::to_lowercase)).collect();
        let mut made = 0;
        for (name, color) in labels {
            if have.contains(&name.to_lowercase()) {
                continue;
            }
            self.post_json(&url, &serde_json::json!({ "displayName": name, "color": outlook_preset(color) }))?;
            made += 1;
        }
        Ok(made)
    }

    /// Makes a folder `name` under the folder `parent` (None: at the mailbox's top, beside the
    /// Inbox), or finds it when it's there already. Returns its id.
    pub fn ensure_folder(&mut self, parent: Option<&str>, name: &str) -> Result<String> {
        let list = match parent {
            Some(id) => format!("{}/mailFolders/{id}/childFolders", self.me()),
            None => format!("{}/mailFolders", self.me()),
        };
        let filter = format!("displayName eq '{}'", name.replace('\'', "''"));
        let mut url = url::Url::parse(&list).map_err(|e| Error::Other(e.to_string()))?;
        url.query_pairs_mut().append_pair("$filter", &filter).append_pair("$select", "id,displayName").append_pair("includeHiddenFolders", "true");
        if let Some(id) = self.get_json(url.as_str())?["value"].as_array().and_then(|v| v.first()).and_then(|f| f["id"].as_str()) {
            return Ok(id.to_string());
        }
        match self.post_json(&list, &serde_json::json!({ "displayName": name })) {
            Ok(v) => v["id"].as_str().map(str::to_string).ok_or_else(|| Error::Other(format!("Microsoft 365 made the folder {name} but gave no id"))),
            // made meanwhile (another computer): find it again
            Err(Error::Other(e)) if e.contains("ErrorFolderExists") => self.get_json(url.as_str())?["value"]
                .as_array()
                .and_then(|v| v.first())
                .and_then(|f| f["id"].as_str())
                .map(str::to_string)
                .ok_or_else(|| Error::Other(format!("the folder {name} exists on the server but can't be found"))),
            Err(e) => Err(e),
        }
    }

    /// Renames a folder (its display name).
    pub fn rename_folder(&mut self, id: &str, name: &str) -> Result<()> {
        self.send_json(true, &format!("{}/mailFolders/{id}", self.me()), &serde_json::json!({ "displayName": name })).map(|_| ())
    }

    /// Moves a folder (with what's in it) into another, e.g. "deleteditems" (as Outlook deletes a
    /// folder): its id there.
    pub fn move_folder(&mut self, id: &str, to: &str) -> Result<String> {
        let v = self.post_json(&format!("{}/mailFolders/{id}/move", self.me()), &serde_json::json!({ "destinationId": to }))?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    /// Whether the user may open this mailbox (a shared one): its Inbox's message count, or
    /// why not (Exchange answers 403 / 404 without access).
    pub fn mailbox_access(&mut self) -> Result<u64> {
        let v = self.get_json(&format!("{}/mailFolders/inbox?$select=totalItemCount", self.me()))?;
        Ok(v["totalItemCount"].as_u64().unwrap_or(0))
    }

    /// Runs a request until it gets an answer. While there's no network (`Error::Network`:
    /// Wi-Fi changed, laptop asleep, hotel portal), it waits — 2, 4, 8 … 30 s between tries,
    /// for as long as it takes — and carries on by itself when the connection is back; the
    /// shared `link` says so meanwhile. Stops (canceled) when the link is stopped.
    fn patiently<T>(&mut self, mut request: impl FnMut(&mut Self) -> Result<T>) -> Result<T> {
        let mut wait = 2;
        loop {
            if self.link.stopped() {
                return Err(Error::Canceled);
            }
            match request(self) {
                Err(Error::Network(why)) => {
                    self.link.went_down(&why);
                    let until = Instant::now() + Duration::from_secs(wait);
                    while Instant::now() < until {
                        if self.link.stopped() {
                            return Err(Error::Canceled);
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    wait = (wait * 2).min(RETRY_MAX);
                }
                answered => {
                    self.link.came_back();
                    return answered;
                }
            }
        }
    }

    /// One GET with the access token (refreshed when due, and once more on a 401), waiting
    /// when Graph says to (429 / 503 / 504, Retry-After).
    fn send_get(&mut self, url: &str) -> Result<ureq::http::Response<ureq::Body>> {
        self.send_get_with(url, None)
    }

    /// A GET with a `Prefer` header (calendars: times in UTC, page sizes).
    fn send_get_with(&mut self, url: &str, prefer: Option<&str>) -> Result<ureq::http::Response<ureq::Body>> {
        let mut refreshed = false;
        for attempt in 0..THROTTLE_TRIES {
            self.refresh_if_needed()?;
            let mut request = self.agent.get(url).header("Authorization", &format!("Bearer {}", self.tokens.access));
            if let Some(p) = prefer {
                request = request.header("Prefer", p);
            }
            let response = request.call().map_err(net)?;
            let status = response.status().as_u16();
            if status == 401 && !refreshed {
                refreshed = true;
                self.tokens.expires = Instant::now();
                continue;
            }
            if matches!(status, 429 | 503 | 504) {
                throttled(&response, attempt);
                continue;
            }
            return Ok(response);
        }
        Err(Error::Other(format!("Microsoft Graph kept asking to wait: {url}")))
    }

    pub fn get_json(&mut self, url: &str) -> Result<serde_json::Value> {
        self.get_json_with(url, None)
    }

    fn get_json_with(&mut self, url: &str, prefer: Option<&str>) -> Result<serde_json::Value> {
        self.patiently(|client| {
            let mut response = client.send_get_with(url, prefer)?;
            let status = response.status();
            let value: serde_json::Value = response.body_mut().with_config().limit(JSON_LIMIT).read_json().map_err(net)?;
            if !status.is_success() {
                let message = value["error"]["message"].as_str().unwrap_or("").to_string();
                return Err(Error::Other(format!("Microsoft Graph {status}: {message}")));
            }
            Ok(value)
        })
    }

    /// Every page of a collection (following @odata.nextLink).
    fn all(&mut self, url: &str) -> Result<Vec<serde_json::Value>> {
        let mut items = Vec::new();
        let mut next = Some(url.to_string());
        while let Some(url) = next {
            let page = self.get_json(&url)?;
            if let Some(values) = page["value"].as_array() {
                items.extend(values.iter().cloned());
            }
            next = page["@odata.nextLink"].as_str().map(str::to_string);
        }
        Ok(items)
    }

    /// The whole folder tree (hidden folders too), depth-first, with message counts.
    pub fn folders(&mut self) -> Result<Vec<MailFolder>> {
        let select = "$select=id,displayName,parentFolderId,childFolderCount,totalItemCount,unreadItemCount,isHidden&$top=250&includeHiddenFolders=true";
        let mut out = Vec::new();
        let top: Vec<MailFolder> = self
            .all(&format!("{}/mailFolders?{select}", self.me()))?
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();
        let mut stack: Vec<MailFolder> = top.into_iter().rev().collect();
        while let Some(folder) = stack.pop() {
            let children: Vec<MailFolder> = if folder.child_folder_count > 0 {
                self.all(&format!("{}/mailFolders/{}/childFolders?{select}", self.me(), folder.id))?
                    .into_iter()
                    .filter_map(|v| serde_json::from_value::<MailFolder>(v).ok())
                    .map(|mut c| {
                        c.depth = folder.depth + 1;
                        c
                    })
                    .collect()
            } else {
                Vec::new()
            };
            out.push(folder);
            stack.extend(children.into_iter().rev());
        }
        Ok(out)
    }

    /// The newest messages of a folder ("inbox", "sentitems", … or a folder id).
    pub fn newest(&mut self, folder: &str, n: usize) -> Result<Vec<MessageSummary>> {
        let page = self.get_json(&format!(
            "{}/mailFolders/{folder}/messages?$top={n}&$select=subject,receivedDateTime,from&$orderby=receivedDateTime%20desc",
            self.me()
        ))?;
        Ok(page["value"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect())
    }

    /// The ids of the user's primary mailbox and Online Archive (mailbox APIs; None: no archive).
    pub fn mailbox_ids(&mut self) -> Result<(Option<String>, Option<String>)> {
        let v = self.get_json(&format!("{GRAPH_BETA}/me/settings/exchange"))?;
        let item = v["value"].as_array().and_then(|a| a.first().cloned()).unwrap_or(v);
        let id = |key: &str| item[key].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        Ok((id("primaryMailboxId"), id("inPlaceArchiveMailboxId")))
    }

    fn folder_page(&mut self, url: &str, with_size: bool) -> Result<Vec<ArchiveFolder>> {
        let expand = format!("$expand=singleValueExtendedProperties($filter=id%20eq%20'{}')", SIZE_PROPERTY.replace(' ', "%20"));
        let sep = if url.contains('?') { '&' } else { '?' };
        let url = if with_size { format!("{url}{sep}{expand}") } else { url.to_string() };
        let values = self.all(&url)?;
        Ok(values
            .into_iter()
            .filter_map(|v| {
                let size = v["singleValueExtendedProperties"]
                    .as_array()
                    .and_then(|props| props.iter().find_map(|p| p["value"].as_str().and_then(|s| s.parse::<u64>().ok())));
                serde_json::from_value::<ArchiveFolder>(v).ok().map(|mut f| {
                    f.size = size;
                    f
                })
            })
            .collect())
    }

    /// A mailbox's folder tree through the mailbox APIs (the Online Archive too), depth-first,
    /// with item counts and, when Exchange gives them, sizes. Folders in an auxiliary archive
    /// mailbox are followed through their `parentMailboxUrl`.
    pub fn mailbox_folders(&mut self, mailbox_id: &str, progress: &dyn Fn(usize)) -> Result<Vec<ArchiveFolder>> {
        let base = format!("{GRAPH_BETA}/admin/exchange/mailboxes/{mailbox_id}");
        let first = format!("{base}/folders?includeHiddenFolders=true");
        // sizes come from an extended property; if Exchange won't expand it here, go without
        let (top, with_size) = match self.folder_page(&first, true) {
            Ok(top) => (top, true),
            Err(_) => (self.folder_page(&first, false)?, false),
        };
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        self.walk_folders(top, &base, with_size, &mut seen, &mut out, progress)?;
        // An auto-expanding archive spreads over auxiliary mailboxes, and a folder can live in
        // one with no folder of the main archive pointing to it, or be split between several:
        // each auxiliary mailbox's whole tree is walked too.
        let mut walked: HashSet<String> = HashSet::from([base.trim_end_matches('/').to_string()]);
        loop {
            let next = out
                .iter()
                .filter_map(|f| f.parent_mailbox_url.as_deref().map(|u| u.trim_end_matches('/').to_string()))
                .find(|u| !walked.contains(u));
            let Some(aux) = next else { break };
            walked.insert(aux.clone());
            let first = format!("{aux}/folders?includeHiddenFolders=true");
            let top = match self.folder_page(&first, with_size) {
                Ok(top) => top,
                Err(_) => self.folder_page(&first, false)?,
            };
            self.walk_folders(top, &aux, with_size, &mut seen, &mut out, progress)?;
        }
        Ok(out)
    }

    /// Depth-first through a mailbox's folders from `top`, each child listed from the mailbox it
    /// really lives in; a folder already listed (reached from another mailbox) is left out.
    fn walk_folders(&mut self, top: Vec<ArchiveFolder>, base: &str, with_size: bool, seen: &mut HashSet<String>, out: &mut Vec<ArchiveFolder>, progress: &dyn Fn(usize)) -> Result<()> {
        let mut stack: Vec<ArchiveFolder> = top.into_iter().rev().collect();
        while let Some(mut folder) = stack.pop() {
            // A folder already listed is never opened again: auxiliary mailboxes point at each
            // other, and opening it again went round forever (2026-10-23: the download stuck at
            // "Reading the Online Archive's folders: 300…" for hours, no new mail meanwhile).
            if !seen.insert(folder.id.clone()) {
                continue;
            }
            if seen.len() > 20_000 {
                return Err(Error::Other("the Online Archive listed more than 20,000 folders: stopped, in case it loops".into()));
            }
            // (where it lives: said by Exchange, else the mailbox being walked)
            folder.parent_mailbox_url.get_or_insert_with(|| base.to_string());
            if folder.child_folder_count > 0 {
                let owner = folder.parent_mailbox_url.clone().unwrap_or_else(|| base.to_string());
                let url = format!("{owner}/folders/{}/childFolders?includeHiddenFolders=true", folder.id);
                let mut children = self.folder_page(&url, with_size)?;
                for c in &mut children {
                    c.depth = folder.depth + 1;
                }
                stack.extend(children.into_iter().rev());
            }
            out.push(folder);
            progress(out.len());
        }
        Ok(())
    }

    /// The base URL of the mailbox a folder really lives in (auxiliary archives have their own).
    pub fn folder_owner(mailbox_id: &str, folder: &ArchiveFolder) -> String {
        folder.parent_mailbox_url.clone().unwrap_or_else(|| format!("{GRAPH_BETA}/admin/exchange/mailboxes/{mailbox_id}"))
    }

    /// Every message of a folder of the primary mailbox: its id and when it was received.
    pub fn message_ids(&mut self, folder_id: &str, progress: &dyn Fn(usize)) -> Result<Vec<(String, Option<String>)>> {
        self.message_ids_since(folder_id, None, progress)
    }

    /// `message_ids`, only messages received since `since` (unix seconds) when given.
    pub fn message_ids_since(&mut self, folder_id: &str, since: Option<i64>, progress: &dyn Fn(usize)) -> Result<Vec<(String, Option<String>)>> {
        Ok(self.listed_since(folder_id, since, progress)?.into_iter().map(|(id, date, _)| (id, date)).collect())
    }

    /// `message_ids_since` with whether each is unread: new mail comes in unread or read as it
    /// is on the server.
    pub fn listed_since(&mut self, folder_id: &str, since: Option<i64>, progress: &dyn Fn(usize)) -> Result<Vec<(String, Option<String>, bool)>> {
        let mut out = Vec::new();
        let filter = since
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|t| format!("&$filter=receivedDateTime%20ge%20{}", t.format("%Y-%m-%dT%H:%M:%SZ")))
            .unwrap_or_default();
        let mut next = Some(format!("{}/mailFolders/{folder_id}/messages?$select=id,receivedDateTime,isRead&$top=1000{filter}", self.me()));
        while let Some(url) = next {
            let page = self.get_json(&url)?;
            for v in page["value"].as_array().into_iter().flatten() {
                if let Some(id) = v["id"].as_str() {
                    out.push((id.to_string(), v["receivedDateTime"].as_str().map(str::to_string), v["isRead"].as_bool() == Some(false)));
                }
            }
            progress(out.len());
            next = page["@odata.nextLink"].as_str().map(str::to_string);
        }
        Ok(out)
    }

    /// A folder's `n` newest messages (id, when received): the quick check for new mail.
    pub fn newest_ids(&mut self, folder_id: &str, n: usize) -> Result<Vec<(String, Option<String>)>> {
        Ok(self.newest_listed(folder_id, n)?.into_iter().map(|(id, date, _)| (id, date)).collect())
    }

    /// `newest_ids` with whether each is unread.
    pub fn newest_listed(&mut self, folder_id: &str, n: usize) -> Result<Vec<(String, Option<String>, bool)>> {
        let page = self.get_json(&format!(
            "{}/mailFolders/{folder_id}/messages?$select=id,receivedDateTime,isRead&$top={n}&$orderby=receivedDateTime%20desc",
            self.me()
        ))?;
        Ok(page["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| Some((v["id"].as_str()?.to_string(), v["receivedDateTime"].as_str().map(str::to_string), v["isRead"].as_bool() == Some(false))))
            .collect())
    }

    /// Every item of a folder (mailbox APIs; the Online Archive too).
    pub fn all_folder_items(&mut self, owner: &str, folder_id: &str, progress: &dyn Fn(usize)) -> Result<Vec<MailboxItem>> {
        let mut out = Vec::new();
        let mut next = Some(format!("{owner}/folders/{folder_id}/items?$top=1000"));
        while let Some(url) = next {
            let page = self.get_json(&url)?;
            out.extend(page["value"].as_array().into_iter().flatten().filter_map(|v| serde_json::from_value::<MailboxItem>(v.clone()).ok()));
            progress(out.len());
            next = page["@odata.nextLink"].as_str().map(str::to_string);
        }
        Ok(out)
    }

    /// The first `n` items of a folder (mailbox APIs).
    pub fn folder_items(&mut self, owner: &str, folder_id: &str, n: usize) -> Result<Vec<MailboxItem>> {
        let page = self.get_json(&format!("{owner}/folders/{folder_id}/items?$top={n}"))?;
        Ok(page["value"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect())
    }

    fn post_json(&mut self, url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.send_json(false, url, body)
    }

    /// A POST (or a PATCH, `patch`) with a JSON body, patiently like the rest.
    fn send_json(&mut self, patch: bool, url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let body = body.to_string();
        self.patiently(|client| {
            let mut refreshed = false;
            for attempt in 0..THROTTLE_TRIES {
                client.refresh_if_needed()?;
                let request = if patch { client.agent.patch(url) } else { client.agent.post(url) };
                let mut response = request
                    .header("Authorization", &format!("Bearer {}", client.tokens.access))
                    .header("Content-Type", "application/json")
                    .send(body.as_bytes())
                    .map_err(net)?;
                let status = response.status().as_u16();
                if status == 401 && !refreshed {
                    refreshed = true;
                    client.tokens.expires = Instant::now();
                    continue;
                }
                if matches!(status, 429 | 503 | 504) {
                    throttled(&response, attempt);
                    continue;
                }
                // (202 Accepted answers have no body: an invitation answered, a meeting canceled)
                let text = response.body_mut().with_config().limit(JSON_LIMIT).read_to_string().map_err(net)?;
                let value: serde_json::Value = if text.trim().is_empty() { serde_json::Value::Null } else { serde_json::from_str(&text).map_err(|e| Error::Other(format!("Microsoft Graph's answer: {e}")))? };
                if !(200..300).contains(&status) {
                    let message = value["error"]["message"].as_str().unwrap_or("").to_string();
                    let code = value["error"]["code"].as_str().unwrap_or("").to_string();
                    return Err(Error::Other(format!("Microsoft Graph {status} {code}: {message}")));
                }
                return Ok(value);
            }
            Err(Error::Other(format!("Microsoft Graph kept asking to wait: {url}")))
        })
    }

    /// Sends a message (MIME) from this client's mailbox: the user's own, or a shared one (Send
    /// As: the mailbox's own address). Exchange keeps the sent copy in that mailbox's Sent Items.
    pub fn send_mime(&mut self, mime: &[u8]) -> Result<()> {
        let url = format!("{}/sendMail", self.me());
        let body = base64::engine::general_purpose::STANDARD.encode(mime);
        self.patiently(|client| {
            let mut refreshed = false;
            for attempt in 0..THROTTLE_TRIES {
                client.refresh_if_needed()?;
                let mut response = client
                    .agent
                    .post(&url)
                    .header("Authorization", &format!("Bearer {}", client.tokens.access))
                    .header("Content-Type", "text/plain")
                    .send(body.as_bytes())
                    .map_err(net)?;
                let status = response.status().as_u16();
                if status == 401 && !refreshed {
                    refreshed = true;
                    client.tokens.expires = Instant::now();
                    continue;
                }
                if matches!(status, 429 | 503 | 504) {
                    throttled(&response, attempt);
                    continue;
                }
                if (200..300).contains(&status) {
                    return Ok(());
                }
                let value: serde_json::Value = response.body_mut().read_json().unwrap_or_default();
                let code = value["error"]["code"].as_str().unwrap_or("");
                let message = value["error"]["message"].as_str().unwrap_or("");
                return Err(Error::Other(format!("Microsoft 365 didn't send it ({status} {code}): {message}")));
            }
            Err(Error::Other("Microsoft 365 kept asking to wait; the message wasn't sent".into()))
        })
    }

    /// A mailbox's folders under `parent` (None: its top), through the mailbox APIs: (id, name).
    pub fn mailbox_children(&mut self, base: &str, parent: Option<&str>) -> Result<Vec<(String, String)>> {
        let mut url = Some(match parent {
            Some(p) => format!("{base}/folders/{p}/childFolders?includeHiddenFolders=true&$top=250"),
            None => format!("{base}/folders?includeHiddenFolders=true&$top=250"),
        });
        let mut out = Vec::new();
        while let Some(u) = url {
            let page = self.get_json(&u)?;
            for f in page["value"].as_array().into_iter().flatten() {
                if let (Some(id), Some(name)) = (f["id"].as_str(), f["displayName"].as_str()) {
                    out.push((id.to_string(), name.to_string()));
                }
            }
            url = page["@odata.nextLink"].as_str().map(str::to_string);
        }
        Ok(out)
    }

    /// Makes a mail folder in a mailbox (mailbox APIs; the Online Archive too) under `parent`
    /// (None: at its top): its id.
    pub fn mailbox_make_folder(&mut self, base: &str, parent: Option<&str>, name: &str) -> Result<String> {
        let url = match parent {
            Some(p) => format!("{base}/folders/{p}/childFolders"),
            None => format!("{base}/folders"),
        };
        let v = self.post_json(&url, &serde_json::json!({ "displayName": name, "type": "IPF.Note" }))?;
        v["id"].as_str().map(str::to_string).ok_or_else(|| Error::Other(format!("Microsoft 365 made the folder {name} but gave no id")))
    }

    /// An import session for a mailbox (mailbox APIs): its pre-signed URL, good for a while.
    pub fn import_session(&mut self, base: &str) -> Result<String> {
        let v = self.post_json(&format!("{base}/createImportSession"), &serde_json::json!({}))?;
        v["importUrl"].as_str().map(str::to_string).ok_or_else(|| Error::Other("Microsoft 365 gave no import URL".into()))
    }

    /// Imports an item (Exchange's export of it) into a folder through an import session's URL
    /// (signed already: no sign-in header): the new item's id.
    pub fn import_item(&mut self, import_url: &str, folder: &str, export: &[u8]) -> Result<String> {
        let body = serde_json::json!({ "FolderId": folder, "Mode": "create", "Data": base64::engine::general_purpose::STANDARD.encode(export) }).to_string();
        self.patiently(|client| {
            for attempt in 0..THROTTLE_TRIES {
                let mut response = client.agent.post(import_url).header("Content-Type", "application/json").send(body.as_bytes()).map_err(net)?;
                let status = response.status().as_u16();
                if matches!(status, 429 | 503 | 504) {
                    throttled(&response, attempt);
                    continue;
                }
                let text = response.body_mut().read_to_string().map_err(net)?;
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                if !(200..300).contains(&status) {
                    return Err(Error::Other(format!("importing into the archive: {status} {}", v["error"]["message"].as_str().unwrap_or(&text))));
                }
                return v["itemId"].as_str().map(str::to_string).ok_or_else(|| Error::Other("the archive took the item but gave no id".into()));
            }
            Err(Error::Other("Microsoft 365 kept asking to wait (importing into the archive)".into()))
        })
    }

    /// Soft-deletes an item of a mailbox folder (mailbox APIs; recoverable). Gone already is fine.
    pub fn mailbox_delete_item(&mut self, owner: &str, folder: &str, item: &str) -> Result<()> {
        self.delete(&format!("{owner}/folders/{folder}/items/{item}?disposalType=softDelete"))
    }

    /// Exports up to 20 items of one mailbox (`owner`: the mailbox's base URL) as Exchange's
    /// Fast Transfer Streams.
    pub fn export_items(&mut self, owner: &str, ids: &[String]) -> Result<Vec<ExportedItem>> {
        let value = self.post_json(&format!("{owner}/exportItems"), &serde_json::json!({ "itemIds": ids }))?;
        let engine = &base64::engine::general_purpose::STANDARD;
        Ok(value["value"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|v| ExportedItem {
                item_id: v["itemId"].as_str().unwrap_or_default().to_string(),
                data: v["data"].as_str().and_then(|d| engine.decode(d).ok()),
                error: v["error"].as_object().map(|e| {
                    format!("{} {}", e.get("code").and_then(|c| c.as_str()).unwrap_or(""), e.get("message").and_then(|c| c.as_str()).unwrap_or(""))
                }),
            })
            .collect())
    }

    /// A message's original bytes (MIME), as the server has them.
    pub fn raw_message(&mut self, id: &str) -> Result<Vec<u8>> {
        let url = format!("{}/messages/{id}/$value", self.me());
        self.patiently(|client| {
            let mut response = client.send_get(&url)?;
            let status = response.status();
            if !status.is_success() {
                return Err(Error::Other(format!("Microsoft Graph {status} for the message's MIME")));
            }
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut response.body_mut().with_config().limit(MESSAGE_LIMIT).reader(), &mut bytes).map_err(|e| {
                // cut off mid-message: the connection broke (unless it's our own size limit)
                if bytes.len() as u64 >= MESSAGE_LIMIT { Error::Other(format!("message larger than {} MB", MESSAGE_LIMIT >> 20)) } else { Error::Network(e.to_string()) }
            })?;
            Ok(bytes)
        })
    }
}

/// Largest message downloaded (bigger ones are reported, not stored).
const MESSAGE_LIMIT: u64 = 200 * 1024 * 1024;

/// Largest JSON answer read. The library's own default (10 MB) refused every archive export of
/// 20 items that held one big attachment, so all 20 failed on every pass: 32,750 messages of
/// the user's Online Archive never came down (found 2026-10-25). An export is base64 (a third
/// bigger): room for a few messages near the size limit; `fetch_archive` asks for items one by
/// one when a group still fails.
const JSON_LIMIT: u64 = 600 * 1024 * 1024;

/// How long Graph asked to wait (Retry-After), else a growing pause; at most 2 minutes.
/// How often, and for how long in all, Microsoft asked Eudora to wait (429 / 503 / 504) since
/// `take_throttling` last asked: these waits were silent, so an hour of them looked like nothing
/// happening.
static THROTTLED: AtomicU64 = AtomicU64::new(0);
static THROTTLED_MS: AtomicU64 = AtomicU64::new(0);

/// Waits as Microsoft asked, counted for the log.
fn throttled(response: &ureq::http::Response<ureq::Body>, attempt: u32) {
    let wait = retry_after(response, attempt);
    THROTTLED.fetch_add(1, Ordering::Relaxed);
    THROTTLED_MS.fetch_add(wait.as_millis() as u64, Ordering::Relaxed);
    std::thread::sleep(wait);
}

/// The waits Microsoft asked for since the last call: (how many, how long in all), or None.
pub fn take_throttling() -> Option<(u64, Duration)> {
    let n = THROTTLED.swap(0, Ordering::Relaxed);
    let ms = THROTTLED_MS.swap(0, Ordering::Relaxed);
    (n > 0).then(|| (n, Duration::from_millis(ms)))
}

/// A line for the log about Microsoft's waits since the last one, if there were any.
pub fn throttling_said() -> Option<String> {
    let (n, waited) = take_throttling()?;
    let secs = waited.as_secs();
    let how_long = if secs >= 120 { format!("{} min", secs / 60) } else { format!("{secs} s") };
    Some(format!(
        "Microsoft asked Eudora to wait {n} time{} ({how_long} of waiting in all, spread over Eudora's requests): too many requests for this mailbox at once. Every computer running Eudora with this account counts toward Microsoft's limit.",
        if n == 1 { "" } else { "s" }
    ))
}

pub(crate) fn retry_after(response: &ureq::http::Response<ureq::Body>, attempt: u32) -> Duration {
    let asked = response.headers().get("Retry-After").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    Duration::from_secs(asked.unwrap_or(1u64 << attempt.min(6)).min(120))
}

/// The Outlook category color ("preset0" … "preset24") nearest to a "#rrggbb" color.
pub fn outlook_preset(color: &str) -> String {
    // Outlook's preset colors as Outlook on the web shows them
    const PRESETS: [(u8, u8, u8); 25] = [
        (0xe7, 0x4c, 0x3c), (0xf3, 0x9c, 0x12), (0xa0, 0x52, 0x2d), (0xf1, 0xc4, 0x0f), (0x4c, 0xaf, 0x50),
        (0x1a, 0xbc, 0x9c), (0x80, 0x80, 0x00), (0x34, 0x98, 0xdb), (0x9b, 0x59, 0xb6), (0xc2, 0x18, 0x5b),
        (0x8c, 0xa6, 0xbe), (0x4a, 0x5f, 0x73), (0x9e, 0x9e, 0x9e), (0x61, 0x61, 0x61), (0x21, 0x21, 0x21),
        (0x8b, 0x00, 0x00), (0xc4, 0x5a, 0x00), (0x5d, 0x40, 0x37), (0xb0, 0x8b, 0x00), (0x1b, 0x5e, 0x20),
        (0x00, 0x60, 0x64), (0x55, 0x6b, 0x2f), (0x0d, 0x47, 0xa1), (0x4a, 0x14, 0x8c), (0x88, 0x0e, 0x4f),
    ];
    let hex = color.trim_start_matches('#');
    let part = |i: usize| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()).unwrap_or(0x80) as i32;
    let (r, g, b) = (part(0), part(2), part(4));
    let nearest = PRESETS
        .iter()
        .enumerate()
        .min_by_key(|(_, (pr, pg, pb))| (r - *pr as i32).pow(2) + (g - *pg as i32).pow(2) + (b - *pb as i32).pow(2))
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("preset{nearest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offline_client() -> GraphClient {
        let tokens = Tokens { access: "test".into(), refresh: "test".into(), expires: Instant::now() + Duration::from_secs(3600) };
        GraphClient { agent: agent(), tokens, scopes: SCOPES, account: String::new(), display_name: String::new(), link: Arc::default(), mailbox: None }
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() // (closed again at once)
    }

    #[test]
    fn a_lost_connection_is_waited_out_and_the_request_carries_on() {
        let port = free_port();
        // the "network" comes back after 3 s: a server that answers one request
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                line.clear();
            }
            let body = r#"{"ok":true}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let mut client = offline_client();
        let link = client.link.clone();
        let saw_lost = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (link, saw) = (link.clone(), saw_lost.clone());
            std::thread::spawn(move || {
                for _ in 0..100 {
                    if link.lost().is_some() {
                        saw.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };
        let value = client.get_json(&format!("http://127.0.0.1:{port}/x")).unwrap();
        server.join().unwrap();
        watcher.join().unwrap();
        assert_eq!(value["ok"], true);
        assert!(saw_lost.load(Ordering::SeqCst), "the outage was shown while it lasted");
        assert!(link.lost().is_none(), "and cleared when the connection came back");
        assert_eq!(link.outages.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stopping_ends_the_wait_for_the_network() {
        let port = free_port();
        let mut client = offline_client();
        let link = client.link.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            link.stop();
        });
        let started = Instant::now();
        assert!(matches!(client.get_json(&format!("http://127.0.0.1:{port}/x")), Err(Error::Canceled)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_stopped_link_works_again_after_resume() {
        // a watching download stops the link at the end of each pass and must resume it for the
        // next, else every check after the first is "canceled" before it starts
        let port = free_port();
        let mut client = offline_client();
        client.link.stop();
        assert!(matches!(client.get_json(&format!("http://127.0.0.1:{port}/x")), Err(Error::Canceled)));
        client.link.resume();
        let server = std::thread::spawn(move || {
            let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut s, &mut buf);
            let body = "{\"ok\":true}";
            let _ = std::io::Write::write_all(&mut s, format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
        });
        std::thread::sleep(Duration::from_millis(200));
        let value = client.get_json(&format!("http://127.0.0.1:{port}/x")).unwrap();
        server.join().unwrap();
        assert_eq!(value["ok"], true, "after resume the request goes through");
    }

    #[test]
    fn pkce_challenge_is_the_rfc_example() {
        // RFC 7636 appendix B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(b64url(&Sha256::digest(verifier.as_bytes())), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn sender_display() {
        let m: MessageSummary = serde_json::from_value(serde_json::json!({
            "id": "1", "from": {"emailAddress": {"name": "Ann Lee", "address": "ann@example.com"}}
        }))
        .unwrap();
        assert_eq!(m.from_display(), "Ann Lee <ann@example.com>");
    }
}

/// Where an archive folder's content went, from Microsoft's "ErrorArchiveFolderMovedPermanently"
/// answer ("…must be redirected to the archive mailbox endpoint https://…/Mailboxes/MBX:…/
/// microsoft.graph.exportItems"): that mailbox's base URL. An auto-expanding archive moves a
/// folder's mail into an auxiliary mailbox; requests for it go there.
pub fn moved_to(error: &str) -> Option<String> {
    if !error.contains("ErrorArchiveFolderMovedPermanently") {
        return None;
    }
    let at = error.find("https://")?;
    let url: String = error[at..].chars().take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | ')' | '<')).collect();
    let url = url.trim_end_matches('.');
    // the mailbox itself: what comes before its folders or an action
    let lower = url.to_ascii_lowercase();
    let end = ["/folders", "/microsoft.graph.", "/exportitems", "/createimportsession"].iter().filter_map(|k| lower.find(k)).min().unwrap_or(url.len());
    Some(url[..end].to_string())
}

#[cfg(test)]
mod moved_tests {
    #[test]
    fn a_moved_archive_folder_names_its_mailbox() {
        let e = "Microsoft Graph 400 ErrorArchiveFolderMovedPermanently: The archive mailbox folder content has been moved to a different location. The request must be redirected to the archive mailbox endpoint https://graph.microsoft.com/beta/admin/Exchange/Mailboxes/MBX:5185312c@0f0f0f0f/microsoft.graph.exportItems.";
        assert_eq!(super::moved_to(e).as_deref(), Some("https://graph.microsoft.com/beta/admin/Exchange/Mailboxes/MBX:5185312c@0f0f0f0f"));
        assert_eq!(super::moved_to("Microsoft Graph 404 ErrorItemNotFound https://x/y"), None);
    }
}

