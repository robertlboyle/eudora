//! Google accounts through Google's own APIs: the Gmail API for mail, the People API for
//! contacts, Google Calendar for calendars, one sign-in asking for all of it ("all steps at
//! once"). Signing in is OAuth with PKCE through the browser back to this computer, as
//! Microsoft's (`graph`); the client is Eudora's "Desktop app" client from Google Cloud, built
//! in when building (`build.rs`: `EUDORA_GOOGLE_CLIENT`). Sign-ins are kept in the credential
//! store (`google:<address>`, the list in `google:accounts`), never in the library.
//!
//! In Testing mode Google ends a sign-in after 7 days: Eudora then says to sign in again.

#[cfg(not(target_os = "ios"))]
use std::net::TcpListener;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::graph::{agent, b64url, keyring_entry, load_secret, net, random, retry_after, store_secret};
#[cfg(not(target_os = "ios"))]
use crate::graph::wait_for_code;

include!(concat!(env!("OUT_DIR"), "/google_client.rs"));

const AUTH: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN: &str = "https://oauth2.googleapis.com/token";
pub const GMAIL: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const PEOPLE: &str = "https://people.googleapis.com/v1";
const CALENDAR: &str = "https://www.googleapis.com/calendar/v3";
/// Everything at once: mail (read, change, send, delete), contacts, calendars.
pub const SCOPES: &str = "openid email https://mail.google.com/ https://www.googleapis.com/auth/contacts https://www.googleapis.com/auth/calendar";
const ACCOUNTS: &str = "google:accounts";

/// One change made in Gmail (on its web site, a phone…): labels added or taken off a message,
/// or the message deleted for good.
#[derive(Clone, Debug, PartialEq)]
pub struct GmailChange {
    pub id: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub deleted: bool,
}

pub struct GoogleClient {
    agent: ureq::Agent,
    access: String,
    expires: Instant,
    refresh: String,
    /// The Google account's address.
    pub account: String,
}

/// Whether this build has Eudora's Google client (`build.rs`); without it Google can't be used.
pub fn available() -> bool {
    client().is_ok()
}

/// Eudora's Google client: on iOS the iOS client (no secret: Google gives an iOS client none),
/// elsewhere the desktop one.
#[cfg(target_os = "ios")]
fn client() -> Result<(&'static str, &'static str)> {
    GOOGLE_IOS_CLIENT.map(|id| (id, "")).ok_or_else(|| Error::Other("this copy of Eudora was built without its Google sign-in for iOS (Google Cloud's iOS client): Gmail through Google's own sign-in isn't available in it yet; an app password over IMAP still works".into()))
}

#[cfg(not(target_os = "ios"))]
fn client() -> Result<(&'static str, &'static str)> {
    GOOGLE_CLIENT.ok_or_else(|| Error::Other("this copy of Eudora was built without its Google sign-in (the Google Cloud client): Gmail through Google's own sign-in isn't available in it; an app password over IMAP still works".into()))
}

#[derive(serde::Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Accounts Gmail asked to slow down, and until when.
static COOLING: std::sync::Mutex<Vec<(String, std::time::SystemTime)>> = std::sync::Mutex::new(Vec::new());

fn cooling_until(account: &str) -> Option<std::time::SystemTime> {
    let now = std::time::SystemTime::now();
    {
        let mut list = COOLING.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|(_, until)| *until > now);
        if let Some((_, until)) = list.iter().find(|(a, _)| a.eq_ignore_ascii_case(account)) {
            return Some(*until);
        }
    }
    // (another Eudora program on this computer, or this one before a restart, may have been told)
    let until = schedule_get(&format!("cool:{}", account.to_lowercase())).map(|s| std::time::UNIX_EPOCH + Duration::from_secs(s.max(0) as u64))?;
    (until > now).then_some(until)
}

fn cool_down(account: &str, until: std::time::SystemTime) {
    {
        let mut list = COOLING.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|(a, _)| !a.eq_ignore_ascii_case(account));
        list.push((account.to_lowercase(), until));
    }
    let secs = until.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    schedule_set(&format!("cool:{}", account.to_lowercase()), secs);
}

/// When each kind of Gmail work was last done, kept on disk in Eudora's folder
/// (`gmail-schedule.json`), so a restart, or the window's and the download's programs both,
/// don't ask Gmail for again what was just asked: its pause ("cool:<address>"), and when the
/// history, calendars, and contacts were last read (`gmail_changes`). Unix seconds by key.
static SCHEDULE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn schedule_path() -> Option<std::path::PathBuf> {
    // (tests never touch the user's own Eudora folder)
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("eudora-test-gmail-schedule-{}.json", std::process::id())));
    }
    crate::library::app_data_dir().map(|d| d.join("gmail-schedule.json"))
}

fn schedule_read() -> std::collections::HashMap<String, i64> {
    schedule_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub(crate) fn schedule_get(key: &str) -> Option<i64> {
    let _held = SCHEDULE.lock().unwrap_or_else(|e| e.into_inner());
    schedule_read().get(key).copied()
}

pub(crate) fn schedule_set(key: &str, secs: i64) {
    let _held = SCHEDULE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(path) = schedule_path() else { return };
    let mut map = schedule_read();
    map.insert(key.to_string(), secs);
    // (written whole, then renamed: another program never reads half of it)
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if let Ok(bytes) = serde_json::to_vec(&map) {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// The time in Gmail's "Retry after 2026-10-27T05:10:52.210Z" (its rate-limit answer).
fn gmail_retry_time(text: &str) -> Option<std::time::SystemTime> {
    let at = text.find("Retry after ")? + "Retry after ".len();
    let stamp: String = text[at..].chars().take_while(|c| !c.is_whitespace() && *c != '"' && *c != ',').collect();
    let t = chrono::DateTime::parse_from_rfc3339(stamp.trim_end_matches('.')).ok()?;
    Some(std::time::UNIX_EPOCH + Duration::from_millis(t.timestamp_millis().max(0) as u64))
}

fn token_request(agent: &ureq::Agent, form: &[(&str, &str)]) -> Result<TokenAnswer> {
    // (an iOS client has no secret: none is sent)
    let form = form.iter().copied().filter(|(k, v)| !(*k == "client_secret" && v.is_empty()));
    let mut response = agent.post(TOKEN).send_form(form).map_err(net)?;
    let answer: TokenAnswer = response.body_mut().read_json().map_err(net)?;
    if let Some(e) = &answer.error {
        let why = answer.error_description.clone().unwrap_or_default();
        return Err(Error::Other(if e == "invalid_grant" {
            format!("Google ended the sign-in ({why}): sign in to Google again (in Google's Testing mode a sign-in lasts 7 days)")
        } else {
            format!("Google refused the sign-in: {e} {why}")
        }));
    }
    Ok(answer)
}

impl GoogleClient {
    /// Signs in through the browser: Google's page (`show` opens it) asks which account and for
    /// the permissions, then comes back to Eudora on this computer. `hint`: the address meant.
    pub fn sign_in(show: &dyn Fn(&str), hint: Option<&str>) -> Result<GoogleClient> {
        let (id, secret) = client()?;
        let agent = agent();
        let verifier = b64url(&random(32)?);
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        let state = b64url(&random(16)?);
        // iOS: Apple's sign-in sheet, back to the iOS client's own address (Google's reversed client id)
        #[cfg(target_os = "ios")]
        let (scheme, redirect) = {
            let scheme = format!("com.googleusercontent.apps.{}", id.trim_end_matches(".apps.googleusercontent.com"));
            let redirect = format!("{scheme}:/oauthredirect");
            (scheme, redirect)
        };
        #[cfg(not(target_os = "ios"))]
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| Error::Other(format!("can't listen for the sign-in: {e}")))?;
        #[cfg(not(target_os = "ios"))]
        let redirect = format!("http://127.0.0.1:{}", listener.local_addr().map_err(|e| Error::Other(e.to_string()))?.port());
        let mut url = url::Url::parse(AUTH).unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect)
            .append_pair("scope", SCOPES)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            // a refresh token every time (Google gives one only with consent shown)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent select_account")
            .append_pair("include_granted_scopes", "true");
        if let Some(address) = hint.map(str::trim).filter(|h| h.contains('@')) {
            url.query_pairs_mut().append_pair("login_hint", address);
        }
        #[cfg(target_os = "ios")]
        let code = {
            let _ = show;
            crate::web_auth::code_in(&crate::web_auth::authenticate(url.as_str(), &scheme)?, &state)?
        };
        #[cfg(not(target_os = "ios"))]
        let code = {
            show(url.as_str());
            wait_for_code(&listener, &state)?
        };
        let answer = token_request(
            &agent,
            &[("client_id", id), ("client_secret", secret), ("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", &redirect), ("code_verifier", &verifier)],
        )?;
        let (Some(access), Some(refresh)) = (answer.access_token, answer.refresh_token) else {
            return Err(Error::Other("Google signed in but gave Eudora no lasting sign-in: try again".into()));
        };
        let mut client = GoogleClient { agent, access, expires: Instant::now() + Duration::from_secs(answer.expires_in.unwrap_or(3600).saturating_sub(120)), refresh, account: String::new() };
        let profile = client.get_json(&format!("{GMAIL}/profile"))?;
        client.account = profile["emailAddress"].as_str().unwrap_or("").to_lowercase();
        if client.account.is_empty() {
            return Err(Error::Other("Google didn't say which account signed in".into()));
        }
        if let Some(meant) = hint.map(str::trim).filter(|h| h.contains('@')) {
            if !meant.eq_ignore_ascii_case(&client.account) {
                return Err(Error::Other(format!("Google signed in {} instead of {meant}: sign in again and choose {meant}", client.account)));
            }
        }
        client.remember()?;
        Ok(client)
    }

    /// A Google account signed in on this computer, from its kept sign-in.
    pub fn resume(account: &str) -> Result<GoogleClient> {
        let (id, secret) = client()?;
        let account = account.to_lowercase();
        let refresh = load_secret(&format!("google:{account}"))
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| Error::Other(format!("{account} isn't signed in to Google on this computer: Settings ▸ Accounts ▸ Add a Google account")))?;
        let agent = agent();
        let answer = token_request(&agent, &[("client_id", id), ("client_secret", secret), ("grant_type", "refresh_token"), ("refresh_token", &refresh)])?;
        let access = answer.access_token.ok_or_else(|| Error::Other("Google gave no access".into()))?;
        let client = GoogleClient { agent, access, expires: Instant::now() + Duration::from_secs(answer.expires_in.unwrap_or(3600).saturating_sub(120)), refresh: answer.refresh_token.unwrap_or(refresh), account };
        client.remember()?;
        Ok(client)
    }

    fn remember(&self) -> Result<()> {
        store_secret(&format!("google:{}", self.account), self.refresh.as_bytes())?;
        let mut all = signed_in_accounts();
        if !all.iter().any(|a| a.eq_ignore_ascii_case(&self.account)) {
            all.push(self.account.clone());
            keyring_entry(ACCOUNTS)?.set_password(&serde_json::to_string(&all)?).map_err(|e| Error::Other(format!("credential store: {e}")))?;
        }
        Ok(())
    }

    fn refresh_if_needed(&mut self) -> Result<()> {
        if Instant::now() < self.expires {
            return Ok(());
        }
        let (id, secret) = client()?;
        let answer = token_request(&self.agent, &[("client_id", id), ("client_secret", secret), ("grant_type", "refresh_token"), ("refresh_token", &self.refresh)])?;
        self.access = answer.access_token.ok_or_else(|| Error::Other("Google gave no access".into()))?;
        self.expires = Instant::now() + Duration::from_secs(answer.expires_in.unwrap_or(3600).saturating_sub(120));
        if let Some(r) = answer.refresh_token {
            self.refresh = r;
            self.remember()?;
        }
        Ok(())
    }

    /// One request, patiently: the network waited out (2, 4 … 30 s, as long as it takes),
    /// Google's "slow down" (429, 5xx) waited out, an expired access renewed once.
    fn call(&mut self, method: &str, url: &str, body: Option<(&str, Vec<u8>)>) -> Result<Value> {
        let mut wait = Duration::from_secs(2);
        let mut renewed = false;
        let mut attempt = 0u32;
        loop {
            // Gmail asked this account to slow down until then: nothing is asked meanwhile (each
            // request would extend it), every caller told at once
            if let Some(until) = cooling_until(&self.account) {
                return Err(Error::Other(format!(
                    "Gmail asked Eudora to slow down for {}: Eudora asks again after {}",
                    self.account,
                    chrono::DateTime::<chrono::Local>::from(until).format("%-I:%M %p")
                )));
            }
            self.refresh_if_needed()?;
            let auth = format!("Bearer {}", self.access);
            let sent = match (method, &body) {
                ("GET", _) => self.agent.get(url).header("Authorization", &auth).call(),
                ("DELETE", _) => self.agent.delete(url).header("Authorization", &auth).call(),
                (m, Some((kind, bytes))) => {
                    let request = if m == "PATCH" { self.agent.patch(url) } else if m == "PUT" { self.agent.put(url) } else { self.agent.post(url) };
                    request.header("Authorization", &auth).header("Content-Type", *kind).send(bytes.as_slice())
                }
                (_, None) => self.agent.post(url).header("Authorization", &auth).send_empty(),
            };
            let mut response = match sent.map_err(net) {
                Ok(r) => r,
                Err(Error::Network(_)) if attempt < 1000 => {
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_secs(30));
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(e),
            };
            let status = response.status().as_u16();
            if status == 401 && !renewed {
                renewed = true;
                self.expires = Instant::now();
                continue;
            }
            if status >= 500 && attempt < 40 {
                std::thread::sleep(retry_after(&response, attempt));
                attempt += 1;
                continue;
            }
            let pause = retry_after(&response, attempt);
            let text = response.body_mut().read_to_string().unwrap_or_default();
            // too many requests (429, or 403 "rateLimitExceeded"): Gmail names when to ask
            // again ("Retry after 2026-10-27T05:10:52Z"); a short wait is waited out, a long one
            // ends this request and stops the account's requests until then (2026-10-27: retrying
            // every few seconds kept the account blocked, and every request waited)
            if status == 429 || (status == 403 && (text.contains("rateLimitExceeded") || text.contains("userRateLimitExceeded"))) {
                let until = gmail_retry_time(&text).unwrap_or_else(|| std::time::SystemTime::now() + pause);
                let wait_for = until.duration_since(std::time::SystemTime::now()).unwrap_or_default();
                if wait_for <= Duration::from_secs(30) && attempt < 5 {
                    std::thread::sleep(wait_for.max(Duration::from_secs(1)));
                    attempt += 1;
                    continue;
                }
                cool_down(&self.account, until.max(std::time::SystemTime::now() + Duration::from_secs(60)));
                continue; // (said by the check at the top)
            }
            let value: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) };
            if !(200..300).contains(&status) {
                let message = value["error"]["message"].as_str().unwrap_or(&text).to_string();
                return Err(Error::Other(format!("Google {status}: {message}")));
            }
            return Ok(value);
        }
    }

    pub fn get_json(&mut self, url: &str) -> Result<Value> {
        self.call("GET", url, None)
    }

    /// A JSON request (POST) and its answer.
    pub fn post_json(&mut self, url: &str, body: &Value) -> Result<Value> {
        self.call("POST", url, Some(("application/json", serde_json::to_vec(body)?)))
    }

    // ------------------------------------------------------------------ Gmail

    /// The account's labels (id, name, kind "system" / "user"), as Gmail keeps them.
    pub fn labels(&mut self) -> Result<Vec<(String, String, String)>> {
        let v = self.get_json(&format!("{GMAIL}/labels"))?;
        Ok(v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| Some((l["id"].as_str()?.to_string(), l["name"].as_str()?.to_string(), l["type"].as_str().unwrap_or("user").to_string())))
            .collect())
    }

    /// How many messages a label holds (Gmail's own count).
    pub fn label_total(&mut self, label: &str) -> Result<u64> {
        Ok(self.get_json(&format!("{GMAIL}/labels/{}", urlencode(label)))?["messagesTotal"].as_u64().unwrap_or(0))
    }

    /// Every message's id with a label (newest first), page by page (`progress`: how many so far).
    /// `label` "": every message (All Mail, without Spam and Trash).
    pub fn message_ids(&mut self, label: &str, progress: &dyn Fn(usize)) -> Result<Vec<String>> {
        self.message_ids_since(label, None, progress)
    }

    /// `message_ids`, only messages received since `since` (unix seconds) when given.
    pub fn message_ids_since(&mut self, label: &str, since: Option<i64>, progress: &dyn Fn(usize)) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut url = if label.is_empty() {
                format!("{GMAIL}/messages?maxResults=500")
            } else {
                format!("{GMAIL}/messages?labelIds={}&maxResults=500&includeSpamTrash=true", urlencode(label))
            };
            if let Some(t) = since {
                url.push_str(&format!("&q={}", urlencode(&format!("after:{t}"))));
            }
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", urlencode(p)));
            }
            let v = self.get_json(&url)?;
            ids.extend(v["messages"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_string)));
            progress(ids.len());
            match v["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_string()),
                None => return Ok(ids),
            }
        }
    }

    /// The newest message ids with a label (a quick look for new mail).
    pub fn newest_ids(&mut self, label: &str, n: usize) -> Result<Vec<String>> {
        let v = self.get_json(&format!("{GMAIL}/messages?labelIds={}&maxResults={n}&includeSpamTrash=true", urlencode(label)))?;
        Ok(v["messages"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_string)).collect())
    }

    /// A message: its original bytes (RFC 822) and the labels it has.
    pub fn raw_message(&mut self, id: &str) -> Result<(Vec<u8>, Vec<String>)> {
        use base64::Engine;
        let v = self.get_json(&format!("{GMAIL}/messages/{id}?format=raw"))?;
        let raw = v["raw"].as_str().ok_or_else(|| Error::Other(format!("Gmail sent message {id} without its bytes")))?;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw.trim_end_matches('='))
            .map_err(|e| Error::Other(format!("Gmail message {id}: {e}")))?;
        let labels = v["labelIds"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_string)).collect();
        Ok((bytes, labels))
    }

    /// Sends a message (its MIME); Gmail keeps the sent copy in Sent Mail.
    pub fn send(&mut self, mime: &[u8]) -> Result<String> {
        use base64::Engine;
        let body = serde_json::json!({ "raw": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mime) }).to_string();
        let v = self.call("POST", &format!("{GMAIL}/messages/send"), Some(("application/json", body.into_bytes())))?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    /// Adds and takes off labels (a move between "folders", read / unread, starred…).
    pub fn modify(&mut self, id: &str, add: &[&str], remove: &[&str]) -> Result<()> {
        let body = serde_json::json!({ "addLabelIds": add, "removeLabelIds": remove }).to_string();
        self.call("POST", &format!("{GMAIL}/messages/{id}/modify"), Some(("application/json", body.into_bytes()))).map(|_| ())
    }

    /// To Gmail's Trash (Empty Trash there deletes it for good after 30 days, as Gmail does).
    pub fn trash(&mut self, id: &str) -> Result<()> {
        self.call("POST", &format!("{GMAIL}/messages/{id}/trash"), None).map(|_| ())
    }

    /// Renames a user label (a folder renamed in Eudora).
    pub fn rename_label(&mut self, id: &str, name: &str) -> Result<()> {
        let body = serde_json::json!({ "name": name }).to_string();
        self.call("PATCH", &format!("{GMAIL}/labels/{}", urlencode(id)), Some(("application/json", body.into_bytes()))).map(|_| ())
    }

    /// Deletes a user label (its messages keep their other labels; All Mail keeps them).
    pub fn delete_label(&mut self, id: &str) -> Result<()> {
        self.call("DELETE", &format!("{GMAIL}/labels/{}", urlencode(id)), None).map(|_| ())
    }

    /// Adds and takes off labels on many messages at once (up to 1,000 a request).
    pub fn batch_modify(&mut self, ids: &[String], add: &[&str], remove: &[&str]) -> Result<()> {
        for chunk in ids.chunks(1000) {
            let body = serde_json::json!({ "ids": chunk, "addLabelIds": add, "removeLabelIds": remove }).to_string();
            self.call("POST", &format!("{GMAIL}/messages/batchModify"), Some(("application/json", body.into_bytes())))?;
        }
        Ok(())
    }

    /// The mailbox's history id now (where reading changes back starts).
    pub fn history_now(&mut self) -> Result<String> {
        let v = self.get_json(&format!("{GMAIL}/profile"))?;
        v["historyId"].as_str().map(str::to_string).ok_or_else(|| Error::Other("Gmail gave no history id".into()))
    }

    /// What changed in the mailbox since `start` (labels added and taken off, messages deleted
    /// for good), and the history id to start from next time. Err "too old" when Gmail no longer
    /// keeps history that far back (then start again from now).
    pub fn history_since(&mut self, start: &str) -> Result<(String, Vec<GmailChange>)> {
        let mut changes = Vec::new();
        let mut page: Option<String> = None;
        let mut latest = start.to_string();
        loop {
            let mut url = format!(
                "{GMAIL}/history?startHistoryId={}&maxResults=500&historyTypes=labelAdded&historyTypes=labelRemoved&historyTypes=messageDeleted",
                urlencode(start)
            );
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", urlencode(p)));
            }
            let v = match self.get_json(&url) {
                Ok(v) => v,
                Err(e) if e.to_string().contains("Google 404") => return Err(Error::Other("too old".into())),
                Err(e) => return Err(e),
            };
            if let Some(h) = v["historyId"].as_str() {
                latest = h.to_string();
            }
            for h in v["history"].as_array().into_iter().flatten() {
                let ids = |key: &str| -> Vec<(String, Vec<String>)> {
                    h[key]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|x| {
                            let id = x["message"]["id"].as_str()?.to_string();
                            let labels = x["labelIds"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_string)).collect();
                            Some((id, labels))
                        })
                        .collect()
                };
                for (id, labels) in ids("labelsAdded") {
                    changes.push(GmailChange { id, added: labels, removed: Vec::new(), deleted: false });
                }
                for (id, labels) in ids("labelsRemoved") {
                    changes.push(GmailChange { id, added: Vec::new(), removed: labels, deleted: false });
                }
                for (id, _) in ids("messagesDeleted") {
                    changes.push(GmailChange { id, added: Vec::new(), removed: Vec::new(), deleted: true });
                }
            }
            match v["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_string()),
                None => return Ok((latest, changes)),
            }
        }
    }

    /// Messages added to the mailbox since `start` (new mail, sent mail, drafts…): their ids, and
    /// the history id to start from next time. One request when nothing changed: what a check for
    /// new mail asks, in place of listing every label. Err "too old" when Gmail no longer keeps
    /// history that far.
    pub fn history_added(&mut self, start: &str) -> Result<(String, Vec<String>)> {
        let mut added = Vec::new();
        let mut page: Option<String> = None;
        let mut latest = start.to_string();
        loop {
            let mut url = format!("{GMAIL}/history?startHistoryId={}&maxResults=500&historyTypes=messageAdded", urlencode(start));
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", urlencode(p)));
            }
            let v = match self.get_json(&url) {
                Ok(v) => v,
                Err(e) if e.to_string().contains("Google 404") => return Err(Error::Other("too old".into())),
                Err(e) => return Err(e),
            };
            if let Some(h) = v["historyId"].as_str() {
                latest = h.to_string();
            }
            for h in v["history"].as_array().into_iter().flatten() {
                for x in h["messagesAdded"].as_array().into_iter().flatten() {
                    if let Some(id) = x["message"]["id"].as_str() {
                        if !added.iter().any(|a| a == id) {
                            added.push(id.to_string());
                        }
                    }
                }
            }
            match v["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_string()),
                None => return Ok((latest, added)),
            }
        }
    }

    /// Out of Gmail's Trash (moved out of the Trash in Eudora).
    pub fn untrash(&mut self, id: &str) -> Result<()> {
        self.call("POST", &format!("{GMAIL}/messages/{id}/untrash"), None).map(|_| ())
    }

    /// Makes a label that shows nowhere in Gmail (sync's own); returns its id.
    pub fn make_hidden_label(&mut self, name: &str) -> Result<String> {
        let body = serde_json::json!({ "name": name, "labelListVisibility": "labelHide", "messageListVisibility": "hide" }).to_string();
        let v = self.call("POST", &format!("{GMAIL}/labels"), Some(("application/json", body.into_bytes())))?;
        v["id"].as_str().map(str::to_string).ok_or_else(|| Error::Other("Gmail made the label but gave no id".into()))
    }

    /// Puts a message into the mailbox with these labels, as is (not sent, not in the Inbox);
    /// returns its id.
    pub fn insert_message(&mut self, mime: &[u8], labels: &[&str]) -> Result<String> {
        use base64::Engine;
        let body = serde_json::json!({ "raw": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mime), "labelIds": labels }).to_string();
        let v = self.call("POST", &format!("{GMAIL}/messages?internalDateSource=dateHeader"), Some(("application/json", body.into_bytes())))?;
        v["id"].as_str().map(str::to_string).ok_or_else(|| Error::Other("Gmail stored the message but gave no id".into()))
    }

    /// Deletes a message for good (sync's own items only: mail goes to the Trash).
    pub fn delete_message(&mut self, id: &str) -> Result<()> {
        self.call("DELETE", &format!("{GMAIL}/messages/{id}"), None).map(|_| ())
    }

    /// Makes a user label (a folder made in Eudora); returns its id.
    pub fn make_label(&mut self, name: &str) -> Result<String> {
        let body = serde_json::json!({ "name": name, "labelListVisibility": "labelShow", "messageListVisibility": "show" }).to_string();
        let v = self.call("POST", &format!("{GMAIL}/labels"), Some(("application/json", body.into_bytes())))?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    /// Makes a contact (Google's person); returns it as Google keeps it now (`as_recorded`).
    pub fn create_contact(&mut self, person: &Value) -> Result<Value> {
        let v = self.call("POST", &format!("{PEOPLE}/people:createContact?personFields={PERSON_FIELDS}"), Some(("application/json", serde_json::to_vec(person)?)))?;
        Ok(as_recorded(v))
    }

    /// Changes a contact (`etag`: the version Eudora has; Google refuses a stale one, and the
    /// change is tried again after the next read).
    pub fn update_contact(&mut self, resource: &str, etag: &str, person: &Value) -> Result<Value> {
        let mut body = person.clone();
        body["etag"] = Value::String(etag.to_string());
        let v = self.call(
            "PATCH",
            &format!("{PEOPLE}/{resource}:updateContact?updatePersonFields={EDITED_FIELDS}&personFields={PERSON_FIELDS}"),
            Some(("application/json", serde_json::to_vec(&body)?)),
        )?;
        Ok(as_recorded(v))
    }

    /// Deletes a contact.
    pub fn delete_contact(&mut self, resource: &str) -> Result<()> {
        self.call("DELETE", &format!("{PEOPLE}/{resource}:deleteContact"), None).map(|_| ())
    }

    // ------------------------------------------------------------------ calendars

    /// The account's calendars (its calendar list), each in Graph's shape (`calendar_as_graph`).
    pub fn calendars(&mut self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut url = format!("{CALENDAR}/users/me/calendarList?maxResults=250");
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", urlencode(p)));
            }
            let v = self.get_json(&url)?;
            out.extend(v["items"].as_array().into_iter().flatten().filter(|c| !c["deleted"].as_bool().unwrap_or(false)).map(calendar_as_graph));
            match v["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_string()),
                None => return Ok(out),
            }
        }
    }

    /// A calendar's events from `from` to `to` (Unix seconds; every occurrence its own), each in
    /// Graph's shape (`event_as_graph`).
    pub fn events(&mut self, calendar: &str, from: i64, to: i64) -> Result<Vec<Value>> {
        let time = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default();
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut url = format!(
                "{CALENDAR}/calendars/{}/events?singleEvents=true&showDeleted=false&maxResults=2500&timeMin={}&timeMax={}",
                urlencode(calendar),
                urlencode(&time(from)),
                urlencode(&time(to))
            );
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", urlencode(p)));
            }
            let v = self.get_json(&url)?;
            out.extend(v["items"].as_array().into_iter().flatten().map(event_as_graph));
            match v["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_string()),
                None => return Ok(out),
            }
        }
    }

    /// An event as Google keeps it.
    pub fn event_get(&mut self, calendar: &str, id: &str) -> Result<Value> {
        self.get_json(&format!("{CALENDAR}/calendars/{}/events/{}", urlencode(calendar), urlencode(id)))
    }

    /// Makes an event (the people asked are sent Google's invitations; a Meet when asked).
    pub fn event_insert(&mut self, calendar: &str, body: &Value) -> Result<Value> {
        let url = format!("{CALENDAR}/calendars/{}/events?sendUpdates=all&conferenceDataVersion=1", urlencode(calendar));
        self.call("POST", &url, Some(("application/json", serde_json::to_vec(body)?)))
    }

    /// Changes an event (its people told when `tell`).
    pub fn event_patch(&mut self, calendar: &str, id: &str, body: &Value, tell: bool) -> Result<Value> {
        let url = format!("{CALENDAR}/calendars/{}/events/{}?sendUpdates={}&conferenceDataVersion=1", urlencode(calendar), urlencode(id), if tell { "all" } else { "none" });
        self.call("PATCH", &url, Some(("application/json", serde_json::to_vec(body)?)))
    }

    /// Deletes an event (cancelled: its people told, as Google Calendar does; else quietly).
    pub fn event_delete(&mut self, calendar: &str, id: &str, tell: bool) -> Result<()> {
        let url = format!("{CALENDAR}/calendars/{}/events/{}?sendUpdates={}", urlencode(calendar), urlencode(id), if tell { "all" } else { "none" });
        self.call("DELETE", &url, None).map(|_| ())
    }

    /// Answers an invitation (accept, tentative, decline) with an optional note to the organizer.
    pub fn event_respond(&mut self, calendar: &str, id: &str, answer: &str, comment: Option<&str>) -> Result<Value> {
        let event = self.event_get(calendar, id)?;
        let status = match answer {
            "accept" => "accepted",
            "tentative" => "tentative",
            _ => "declined",
        };
        let mut attendees = event["attendees"].as_array().cloned().unwrap_or_default();
        let Some(me) = attendees.iter_mut().find(|a| a["self"].as_bool().unwrap_or(false)) else {
            return Err(Error::Other("you're not on this event's list of people: nothing to answer".into()));
        };
        me["responseStatus"] = status.into();
        if let Some(c) = comment.filter(|c| !c.trim().is_empty()) {
            me["comment"] = c.into();
        }
        self.event_patch(calendar, id, &serde_json::json!({ "attendees": attendees }), true)
    }

    // ------------------------------------------------------------------ contacts

    /// Every contact (People API connections), as Google gives them.
    pub fn contacts(&mut self) -> Result<Vec<Value>> {
        let fields = "names,nicknames,emailAddresses,phoneNumbers,addresses,organizations,urls,birthdays,biographies,memberships,metadata";
        let mut all = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut url = format!("{PEOPLE}/people/me/connections?pageSize=1000&personFields={fields}");
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", urlencode(p)));
            }
            let v = self.get_json(&url)?;
            all.extend(v["connections"].as_array().into_iter().flatten().cloned());
            match v["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_string()),
                None => return Ok(all),
            }
        }
    }
}

/// A Google calendar in the shape Eudora reads Microsoft's (`calendar::record_calendars`): one
/// calendar code for both. Editable when the account owns it or may change it (Google's
/// accessRole owner / writer); others' calendars shared to read only stay read-only.
pub fn calendar_as_graph(c: &Value) -> Value {
    serde_json::json!({
        "id": c["id"],
        "name": c["summaryOverride"].as_str().or(c["summary"].as_str()).unwrap_or("Calendar"),
        "hexColor": c["backgroundColor"],
        "canEdit": matches!(c["accessRole"].as_str(), Some("owner" | "writer")),
        "isDefaultCalendar": c["primary"].as_bool().unwrap_or(false),
    })
}

/// An event change as Eudora makes it (Graph's shape: `meetings::new_event_json`, the window's
/// edits) in Google Calendar's shape: only the fields it has.
pub fn event_from_graph_patch(p: &Value) -> Value {
    let mut g = serde_json::Map::new();
    if let Some(s) = p["subject"].as_str() {
        g.insert("summary".into(), s.into());
    }
    if let Some(c) = p["body"]["content"].as_str() {
        g.insert("description".into(), c.into());
    }
    if let Some(l) = p["location"]["displayName"].as_str() {
        g.insert("location".into(), l.into());
    }
    let all_day = p["isAllDay"].as_bool();
    for key in ["start", "end"] {
        if let Some(t) = p[key]["dateTime"].as_str() {
            // (Eudora's times are UTC: "2026-11-03T15:00:00")
            let utc = t.trim_end_matches('Z');
            let value = if all_day == Some(true) {
                serde_json::json!({ "date": utc.get(..10).unwrap_or(utc) })
            } else {
                serde_json::json!({ "dateTime": format!("{utc}Z"), "timeZone": "UTC" })
            };
            g.insert(key.into(), value);
        }
    }
    if let Some(list) = p["attendees"].as_array() {
        let people: Vec<Value> = list
            .iter()
            .filter_map(|a| {
                let email = a["emailAddress"]["address"].as_str()?;
                Some(serde_json::json!({ "email": email, "displayName": a["emailAddress"]["name"].as_str().unwrap_or(""), "optional": a["type"] == "optional" }))
            })
            .collect();
        g.insert("attendees".into(), people.into());
    }
    if let Some(s) = p["showAs"].as_str() {
        g.insert("transparency".into(), if s == "free" { "transparent" } else { "opaque" }.into());
    }
    if let Some(s) = p["sensitivity"].as_str() {
        g.insert("visibility".into(), if s == "private" || s == "confidential" { "private" } else { "default" }.into());
    }
    if let Some(on) = p["isReminderOn"].as_bool() {
        let minutes = p["reminderMinutesBeforeStart"].as_i64().unwrap_or(10);
        let overrides = if on { vec![serde_json::json!({ "method": "popup", "minutes": minutes })] } else { vec![] };
        g.insert("reminders".into(), serde_json::json!({ "useDefault": false, "overrides": overrides }));
    }
    if p[crate::meetings::GOOGLE_MEET].as_bool() == Some(true) {
        let request: String = crate::graph::random(8).unwrap_or_default().iter().map(|b| format!("{b:02x}")).collect();
        g.insert("conferenceData".into(), serde_json::json!({ "createRequest": { "requestId": request, "conferenceSolutionKey": { "type": "hangoutsMeet" } } }));
    }
    Value::Object(g)
}

/// A Google time ({dateTime with an offset} or {date} for all day) as Graph's UTC form, and
/// whether it's all day.
fn time_as_graph(t: &Value) -> (Value, bool) {
    if let Some(dt) = t["dateTime"].as_str() {
        let utc = chrono::DateTime::parse_from_rfc3339(dt).map(|d| d.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%S").to_string()).unwrap_or_default();
        (serde_json::json!({ "dateTime": utc, "timeZone": "UTC" }), false)
    } else {
        // (all day: its date's midnight, as Eudora keeps all-day events)
        let day = t["date"].as_str().unwrap_or("");
        (serde_json::json!({ "dateTime": format!("{day}T00:00:00"), "timeZone": "UTC" }), true)
    }
}

/// A Google answer as Graph's word for it.
fn response_as_graph(r: &str) -> &'static str {
    match r {
        "accepted" => "accepted",
        "declined" => "declined",
        "tentative" => "tentativelyAccepted",
        _ => "none",
    }
}

/// A Google event in the shape Eudora reads Microsoft's (`calendar::from_graph`).
pub fn event_as_graph(e: &Value) -> Value {
    let (start, all_day) = time_as_graph(&e["start"]);
    let (end, _) = time_as_graph(&e["end"]);
    let attendees: Vec<Value> = e["attendees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| !a["resource"].as_bool().unwrap_or(false))
        .map(|a| {
            serde_json::json!({
                "emailAddress": { "name": a["displayName"].as_str().unwrap_or(""), "address": a["email"].as_str().unwrap_or("") },
                "type": if a["optional"].as_bool().unwrap_or(false) { "optional" } else { "required" },
                "status": { "response": response_as_graph(a["responseStatus"].as_str().unwrap_or("")) },
            })
        })
        .collect();
    let mine = e["attendees"].as_array().into_iter().flatten().find(|a| a["self"].as_bool().unwrap_or(false));
    let organizer_is_me = e["organizer"]["self"].as_bool().unwrap_or(false);
    let response = match (organizer_is_me, mine) {
        (true, _) => "organizer",
        (false, Some(a)) => response_as_graph(a["responseStatus"].as_str().unwrap_or("")),
        (false, None) => "none",
    };
    let join = e["hangoutLink"].as_str().map(str::to_string).or_else(|| {
        e["conferenceData"]["entryPoints"].as_array().into_iter().flatten().find(|p| p["entryPointType"] == "video").and_then(|p| p["uri"].as_str().map(str::to_string))
    });
    let reminder = e["reminders"]["overrides"].as_array().and_then(|o| o.iter().filter_map(|r| r["minutes"].as_i64()).min());
    let description = e["description"].as_str().unwrap_or("");
    serde_json::json!({
        "id": e["id"],
        "changeKey": e["etag"],
        "iCalUId": e["iCalUID"],
        "subject": e["summary"].as_str().unwrap_or("(no title)"),
        "start": start,
        "end": end,
        "isAllDay": all_day,
        "location": { "displayName": e["location"].as_str().unwrap_or("") },
        "organizer": { "emailAddress": { "name": e["organizer"]["displayName"].as_str().unwrap_or(""), "address": e["organizer"]["email"].as_str().unwrap_or("") } },
        "attendees": attendees,
        "isOrganizer": organizer_is_me,
        "responseStatus": { "response": response },
        "showAs": if e["transparency"] == "transparent" { "free" } else { "busy" },
        "isReminderOn": reminder.is_some() || e["reminders"]["useDefault"].as_bool().unwrap_or(false),
        "reminderMinutesBeforeStart": reminder.unwrap_or(10),
        "sensitivity": if matches!(e["visibility"].as_str(), Some("private" | "confidential")) { "private" } else { "normal" },
        "isCancelled": e["status"] == "cancelled",
        "onlineMeeting": { "joinUrl": join },
        "type": if e["recurringEventId"].is_string() { "occurrence" } else { "singleInstance" },
        "seriesMasterId": e["recurringEventId"],
        "webLink": e["htmlLink"],
        // (the description may be HTML: kept short, as Graph's preview is)
        "bodyPreview": crate::text_repair::clean(&mail_parser::decoders::html::html_to_text(description), 2000),
        // (the whole text, for editing: Google's description is HTML or plain text)
        "body": { "contentType": "html", "content": description },
    })
}

/// The Google accounts signed in on this computer.
pub fn signed_in_accounts() -> Vec<String> {
    keyring_entry(ACCOUNTS).ok().and_then(|e| e.get_password().ok()).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A Google contact (People API "person") read into a card.
/// The fields Eudora reads, and the ones it changes (the others, photos and groups, are left).
const PERSON_FIELDS: &str = "names,nicknames,emailAddresses,phoneNumbers,addresses,organizations,urls,birthdays,biographies,memberships,metadata";
const EDITED_FIELDS: &str = "names,nicknames,emailAddresses,phoneNumbers,addresses,organizations,urls,birthdays,biographies";

/// A person as Google keeps it, with the id and version Eudora records for any contact
/// (`server_changes::record_batch`: "id", "changeKey").
fn as_recorded(mut v: Value) -> Value {
    v["id"] = v["resourceName"].clone();
    v["changeKey"] = v["etag"].clone();
    v
}

/// A contact card as Google's person (`card_from_person` the other way).
pub fn person_from_card(c: &crate::cards::Card) -> Value {
    let kind = |k: &str| match k {
        "mobile" => "mobile",
        "work" => "work",
        "home" => "home",
        "fax" => "workFax",
        _ => "other",
    };
    let birthday = || -> Option<Value> {
        let parts: Vec<i64> = c.birthday.split('-').filter_map(|p| p.parse().ok()).collect();
        match parts.as_slice() {
            [y, m, d] => Some(serde_json::json!({ "date": { "year": y, "month": m, "day": d } })),
            [m, d] => Some(serde_json::json!({ "date": { "month": m, "day": d } })),
            _ => None,
        }
    };
    serde_json::json!({
        "names": [{ "givenName": c.given, "middleName": c.middle, "familyName": c.family, "honorificPrefix": c.prefix, "honorificSuffix": c.suffix, "unstructuredName": c.name }],
        "nicknames": if c.nickname.is_empty() { vec![] } else { vec![serde_json::json!({ "value": c.nickname })] },
        "emailAddresses": c.emails.iter().map(|e| serde_json::json!({ "value": e.value, "type": kind(&e.kind) })).collect::<Vec<_>>(),
        "phoneNumbers": c.phones.iter().map(|p| serde_json::json!({ "value": p.value, "type": kind(&p.kind) })).collect::<Vec<_>>(),
        "addresses": c.addresses.iter().map(|a| serde_json::json!({ "type": kind(&a.kind), "streetAddress": a.street, "city": a.city, "region": a.region, "postalCode": a.postal_code, "country": a.country })).collect::<Vec<_>>(),
        "organizations": if c.company.is_empty() && c.title.is_empty() && c.department.is_empty() { vec![] } else { vec![serde_json::json!({ "name": c.company, "department": c.department, "title": c.title })] },
        "urls": c.websites.iter().map(|u| serde_json::json!({ "value": u })).collect::<Vec<_>>(),
        "birthdays": birthday().into_iter().collect::<Vec<_>>(),
        "biographies": if c.notes.is_empty() { vec![] } else { vec![serde_json::json!({ "value": c.notes, "contentType": "TEXT_PLAIN" })] },
    })
}

pub fn card_from_person(p: &Value) -> crate::cards::Card {
    use crate::cards::{Card, Labeled, Postal};
    let s = |v: &Value| v.as_str().unwrap_or("").trim().to_string();
    let first = |key: &str| p[key].as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
    let name = first("names");
    let org = first("organizations");
    let kind = |t: &Value| match t.as_str().unwrap_or("").to_ascii_lowercase().as_str() {
        "mobile" => "mobile",
        "work" => "work",
        "home" => "home",
        "workfax" | "homefax" => "fax",
        _ => "other",
    }
    .to_string();
    let mut card = Card {
        name: s(&name["displayName"]),
        given: s(&name["givenName"]),
        middle: s(&name["middleName"]),
        family: s(&name["familyName"]),
        prefix: s(&name["honorificPrefix"]),
        suffix: s(&name["honorificSuffix"]),
        nickname: s(&first("nicknames")["value"]),
        company: s(&org["name"]),
        department: s(&org["department"]),
        title: s(&org["title"]),
        notes: s(&first("biographies")["value"]),
        ..Default::default()
    };
    for e in p["emailAddresses"].as_array().into_iter().flatten() {
        let v = s(&e["value"]);
        if !v.is_empty() {
            card.emails.push(Labeled { kind: kind(&e["type"]), value: v });
        }
    }
    for n in p["phoneNumbers"].as_array().into_iter().flatten() {
        let v = s(&n["value"]);
        if !v.is_empty() {
            card.phones.push(Labeled { kind: kind(&n["type"]), value: v });
        }
    }
    for a in p["addresses"].as_array().into_iter().flatten() {
        let postal = Postal {
            kind: kind(&a["type"]),
            street: [s(&a["streetAddress"]), s(&a["extendedAddress"])].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join("\n"),
            city: s(&a["city"]),
            region: s(&a["region"]),
            postal_code: s(&a["postalCode"]),
            country: s(&a["country"]),
        };
        if [&postal.street, &postal.city, &postal.region, &postal.postal_code, &postal.country].iter().any(|x| !x.is_empty()) {
            card.addresses.push(postal);
        }
    }
    card.websites = p["urls"].as_array().into_iter().flatten().map(|u| s(&u["value"])).filter(|u| !u.is_empty()).collect();
    if let Some(d) = p["birthdays"].as_array().and_then(|a| a.first()).map(|b| &b["date"]) {
        if let (Some(m), Some(day)) = (d["month"].as_u64(), d["day"].as_u64()) {
            card.birthday = match d["year"].as_u64() {
                Some(y) => format!("{y:04}-{m:02}-{day:02}"),
                None => format!("--{m:02}-{day:02}"),
            };
        }
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_s_retry_time_is_read_and_an_account_waits_for_it() {
        let text = r#"{"error": {"code": 429, "message": "User-rate limit exceeded.  Retry after 2026-10-27T05:10:52.210Z", "errors": [{"reason": "rateLimitExceeded"}]}}"#;
        let t = gmail_retry_time(text).unwrap();
        assert_eq!(t.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(), chrono::DateTime::parse_from_rfc3339("2026-10-27T05:10:52.210Z").unwrap().timestamp_millis() as u128);
        assert!(gmail_retry_time("no time here").is_none());
        let soon = std::time::SystemTime::now() + Duration::from_secs(600);
        cool_down("Someone@Gmail.com", soon);
        assert_eq!(cooling_until("someone@gmail.com"), Some(soon));
        assert_eq!(cooling_until("other@gmail.com"), None);
        cool_down("someone@gmail.com", std::time::SystemTime::now() - Duration::from_secs(1));
        assert_eq!(cooling_until("someone@gmail.com"), None, "past: over");
    }

    /// What Gmail answers one request on a real account (EUDORA_GMAIL=address): status, its
    /// Retry-After, the start of its body. Never the sign-in.
    #[test]
    #[ignore]
    fn gmail_answer() {
        let address = std::env::var("EUDORA_GMAIL").expect("EUDORA_GMAIL");
        let client = GoogleClient::resume(&address).unwrap();
        let mut response = client.agent.get(&format!("{GMAIL}/labels")).header("Authorization", &format!("Bearer {}", client.access)).call().unwrap();
        let status = response.status().as_u16();
        let retry = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = response.body_mut().read_to_string().unwrap_or_default();
        eprintln!("status {status}, retry-after {retry:?}, body: {}", body.chars().take(400).collect::<String>());
    }

    #[test]
    fn a_card_goes_to_google_and_comes_back_the_same() {
        use crate::cards::{Card, Labeled, Postal};
        let card = Card {
            given: "Ann".into(),
            family: "Smith".into(),
            company: "Example Co".into(),
            title: "CFO".into(),
            emails: vec![Labeled { kind: "work".into(), value: "ann@example.com".into() }],
            phones: vec![Labeled { kind: "mobile".into(), value: "+1 555 0100".into() }],
            addresses: vec![Postal { kind: "home".into(), street: "1 Main St".into(), city: "Springfield".into(), region: "IL".into(), postal_code: "62701".into(), country: "US".into() }],
            notes: "met at the conference".into(),
            ..Default::default()
        };
        let back = card_from_person(&person_from_card(&card));
        assert_eq!((back.given.as_str(), back.family.as_str(), back.company.as_str(), back.title.as_str()), ("Ann", "Smith", "Example Co", "CFO"));
        assert_eq!(back.emails, card.emails);
        assert_eq!(back.phones, card.phones);
        assert_eq!(back.addresses[0].city, "Springfield");
        assert_eq!(back.notes, "met at the conference");
    }

    #[test]
    fn an_appointment_made_in_eudora_goes_to_google_calendar() {
        let p = serde_json::json!({
            "subject": "Board meeting",
            "start": { "dateTime": "2026-11-03T15:00:00", "timeZone": "UTC" },
            "end": { "dateTime": "2026-11-03T16:00:00", "timeZone": "UTC" },
            "isAllDay": false,
            "location": { "displayName": "Room 4" },
            "body": { "contentType": "html", "content": "<p>Agenda</p>" },
            "attendees": [{ "emailAddress": { "name": "Ann", "address": "ann@example.com" }, "type": "required" }],
            "showAs": "free",
            "isReminderOn": true,
            "reminderMinutesBeforeStart": 15,
            "sensitivity": "private",
            crate::meetings::GOOGLE_MEET: true,
        });
        let g = event_from_graph_patch(&p);
        assert_eq!(g["summary"], "Board meeting");
        assert_eq!(g["start"]["dateTime"], "2026-11-03T15:00:00Z");
        assert_eq!(g["attendees"][0]["email"], "ann@example.com");
        assert_eq!((g["transparency"].as_str(), g["visibility"].as_str()), (Some("transparent"), Some("private")));
        assert_eq!(g["reminders"]["overrides"][0]["minutes"], 15);
        assert_eq!(g["conferenceData"]["createRequest"]["conferenceSolutionKey"]["type"], "hangoutsMeet");
        // all day: dates; a change of the subject alone touches nothing else
        let day = event_from_graph_patch(&serde_json::json!({ "isAllDay": true, "start": { "dateTime": "2026-11-03T00:00:00" }, "end": { "dateTime": "2026-11-04T00:00:00" } }));
        assert_eq!((day["start"]["date"].as_str(), day["end"]["date"].as_str()), (Some("2026-11-03"), Some("2026-11-04")));
        let only = event_from_graph_patch(&serde_json::json!({ "subject": "New name" }));
        assert_eq!(only.as_object().unwrap().len(), 1);
        // and back as Eudora reads it, its whole text there for editing
        let back = event_as_graph(&serde_json::json!({ "id": "e1", "etag": "\"3\"", "summary": "Board meeting", "description": "<p>Agenda</p>", "start": { "dateTime": "2026-11-03T10:00:00-05:00" }, "end": { "dateTime": "2026-11-03T11:00:00-05:00" } }));
        assert_eq!((back["id"].as_str(), back["body"]["content"].as_str()), (Some("e1"), Some("<p>Agenda</p>")));
    }

    #[test]
    fn a_google_event_reads_like_a_microsoft_one() {
        let e = serde_json::json!({
            "id": "abc", "etag": "\"1\"", "summary": "Board meeting", "status": "confirmed",
            "start": { "dateTime": "2026-11-03T10:00:00-05:00" }, "end": { "dateTime": "2026-11-03T11:00:00-05:00" },
            "organizer": { "email": "ann@example.com", "displayName": "Ann" },
            "attendees": [{ "email": "me@gmail.com", "self": true, "responseStatus": "tentative" }, { "email": "ann@example.com", "responseStatus": "accepted" }],
            "hangoutLink": "https://meet.google.com/xyz", "transparency": "opaque", "description": "<b>Agenda</b>"
        });
        let ev = crate::calendar::from_graph(&event_as_graph(&e));
        assert_eq!(ev.subject, "Board meeting");
        assert_eq!(ev.start, 1_793_718_000, "10 AM Eastern is 3 PM UTC");
        assert_eq!(ev.end - ev.start, 3600);
        assert!(!ev.all_day);
        assert_eq!(ev.response, "tentativelyAccepted");
        assert_eq!(ev.join_url.as_deref(), Some("https://meet.google.com/xyz"));
        assert_eq!(ev.show_as, "busy");
        assert!(ev.preview.contains("Agenda") && !ev.preview.contains('<'));
        // an all-day event: its date's midnight
        let day = crate::calendar::from_graph(&event_as_graph(&serde_json::json!({ "id": "d", "start": { "date": "2026-12-25" }, "end": { "date": "2026-12-26" } })));
        assert!(day.all_day);
        assert_eq!(day.end - day.start, 86400);
    }

    #[test]
    fn a_google_person_is_read_into_a_card() {
        let p = serde_json::json!({
            "resourceName": "people/c1",
            "names": [{ "displayName": "Ann Smith", "givenName": "Ann", "familyName": "Smith" }],
            "emailAddresses": [{ "value": "ann@x.example", "type": "work" }],
            "phoneNumbers": [{ "value": "+1 555 0100", "type": "mobile" }],
            "organizations": [{ "name": "Acme", "title": "CTO" }],
            "addresses": [{ "streetAddress": "1 Main St", "city": "Rockaway", "region": "NJ", "postalCode": "07866", "type": "home" }],
            "birthdays": [{ "date": { "month": 5, "day": 4 } }]
        });
        let c = card_from_person(&p);
        assert_eq!((c.shown().as_str(), c.company.as_str(), c.title.as_str()), ("Ann Smith", "Acme", "CTO"));
        assert_eq!((c.emails[0].kind.as_str(), c.phones[0].kind.as_str(), c.addresses[0].kind.as_str()), ("work", "mobile", "home"));
        assert_eq!(c.birthday, "--05-04");
        assert_eq!(urlencode("Label_12/a b"), "Label_12%2Fa%20b");
    }
}
