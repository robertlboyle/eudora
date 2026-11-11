//! Zoom meetings made through Zoom's own sign-in. The user makes a Zoom app (Zoom App Marketplace
//! ▸ Develop ▸ Build App ▸ General App, user-managed, OAuth; scope `meeting:write:meeting`;
//! redirect URL `http://localhost:47891/zoom`) and types its Client ID and Client Secret into
//! Settings ▸ Calendar (both kept in the credential store); Sign in to Zoom asks Zoom in the
//! browser, and the lasting sign-in is kept there too. Each appointment's Zoom meeting is then a
//! meeting of its own (its ID, passcode and dial-in in the invitation), else the Personal Meeting
//! Room.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::TcpListener;

use crate::error::{Error, Result};
use crate::graph::{b64url, delete_secret, load_secret, random, store_secret, wait_for_code};
use crate::meetings::Conference;

/// The redirect URL to register in the Zoom app (a fixed port: Zoom checks it exactly).
pub const REDIRECT: &str = "http://localhost:47891/zoom";
const PORT: u16 = 47891;
const APP: &str = "zoom:app";
const REFRESH: &str = "zoom:refresh";

/// The user's Zoom app: its Client ID and Client Secret.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ZoomApp {
    pub id: String,
    pub secret: String,
}

pub fn app() -> Option<ZoomApp> {
    load_secret(APP).and_then(|b| serde_json::from_slice::<ZoomApp>(&b).ok()).filter(|a| !a.id.trim().is_empty() && !a.secret.trim().is_empty())
}

/// Keeps (or with None, forgets) the Zoom app; forgetting it also signs out.
pub fn set_app(app: Option<&ZoomApp>) -> Result<()> {
    match app {
        Some(a) if !a.id.trim().is_empty() && !a.secret.trim().is_empty() => store_secret(APP, &serde_json::to_vec(&ZoomApp { id: a.id.trim().into(), secret: a.secret.trim().into() })?),
        _ => {
            delete_secret(APP);
            delete_secret(REFRESH);
            Ok(())
        }
    }
}

/// Signed in to Zoom on this computer (a lasting sign-in kept).
pub fn signed_in() -> bool {
    app().is_some() && load_secret(REFRESH).is_some()
}

pub fn sign_out() {
    delete_secret(REFRESH);
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(30))).user_agent("Eudora").http_status_as_error(false).build();
    ureq::Agent::new_with_config(config)
}

/// Zoom's token answer: (access, the new refresh token: Zoom gives a new one each time).
fn token(app: &ZoomApp, form: &[(&str, &str)]) -> Result<(String, String)> {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", app.id, app.secret));
    let body: String = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let mut response = agent()
        .post("https://zoom.us/oauth/token")
        .header("Authorization", &format!("Basic {basic}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.as_bytes())
        .map_err(|e| Error::Network(format!("Zoom: {e}")))?;
    let status = response.status().as_u16();
    let v: Value = serde_json::from_str(&response.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
    if status != 200 {
        let why = v["reason"].as_str().or_else(|| v["error_description"].as_str()).or_else(|| v["error"].as_str()).unwrap_or("no reason given");
        return Err(Error::Other(format!("Zoom refused the sign-in ({status}): {why}")));
    }
    match (v["access_token"].as_str(), v["refresh_token"].as_str()) {
        (Some(a), Some(r)) => Ok((a.to_string(), r.to_string())),
        _ => Err(Error::Other("Zoom gave no lasting sign-in".into())),
    }
}

/// Signs in to Zoom through the browser (`show` opens Zoom's page); the lasting sign-in kept.
pub fn sign_in(show: &dyn Fn(&str)) -> Result<()> {
    let app = app().ok_or_else(|| Error::Other("type your Zoom app's Client ID and Client Secret in Settings ▸ Calendar first".into()))?;
    let verifier = b64url(&random(32)?);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let state = b64url(&random(16)?);
    let listener = TcpListener::bind(("127.0.0.1", PORT)).map_err(|e| Error::Other(format!("can't listen for Zoom's answer on port {PORT}: {e}")))?;
    let mut url = url::Url::parse("https://zoom.us/oauth/authorize").unwrap();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &app.id)
        .append_pair("redirect_uri", REDIRECT)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    show(url.as_str());
    let code = wait_for_code(&listener, &state)?;
    let (_, refresh) = token(&app, &[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", REDIRECT), ("code_verifier", &verifier)])?;
    store_secret(REFRESH, refresh.as_bytes())
}

/// An access token from the kept sign-in (the new refresh token kept: Zoom's are single use).
fn access() -> Result<String> {
    let app = app().ok_or_else(|| Error::Other("no Zoom app set (Settings ▸ Calendar)".into()))?;
    let refresh = load_secret(REFRESH).and_then(|b| String::from_utf8(b).ok()).ok_or_else(|| Error::Other("not signed in to Zoom (Settings ▸ Calendar ▸ Sign in to Zoom)".into()))?;
    let (access, next) = token(&app, &[("grant_type", "refresh_token"), ("refresh_token", &refresh)])?;
    store_secret(REFRESH, next.as_bytes())?;
    Ok(access)
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A Zoom meeting of its own for an appointment: its link, ID, passcode and dial-in numbers.
pub fn create_meeting(topic: &str, start: i64, end: i64) -> Result<Conference> {
    let token = access()?;
    let at = chrono::DateTime::from_timestamp(start, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default();
    let minutes = ((end - start) / 60).max(15);
    let body = json!({ "topic": topic, "type": 2, "start_time": at, "duration": minutes, "timezone": "UTC", "settings": { "join_before_host": true, "waiting_room": false } });
    let mut response = agent()
        .post("https://api.zoom.us/v2/users/me/meetings")
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .map_err(|e| Error::Network(format!("Zoom: {e}")))?;
    let status = response.status().as_u16();
    let v: Value = serde_json::from_str(&response.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return Err(Error::Other(format!("Zoom didn't make the meeting ({status}): {}", v["message"].as_str().unwrap_or("no reason given"))));
    }
    Ok(conference_of(&v))
}

/// Zoom's answer as the meeting's details.
fn conference_of(v: &Value) -> Conference {
    let link = v["join_url"].as_str().unwrap_or("").to_string();
    let id = v["id"].as_i64().map(|n| {
        let s = n.to_string();
        // shown as Zoom does: 123 4567 8901
        if s.len() == 11 { format!("{} {} {}", &s[..3], &s[3..7], &s[7..]) } else { s }
    });
    let passcode = v["password"].as_str().unwrap_or("");
    let mut html = format!("<p><b>Join Zoom meeting</b><br><a href=\"{0}\">{0}</a>", esc(&link));
    if let Some(id) = &id {
        html.push_str(&format!("<br>Meeting ID: {}", esc(id)));
    }
    if !passcode.is_empty() {
        html.push_str(&format!("<br>Passcode: {}", esc(passcode)));
    }
    html.push_str("</p>");
    let numbers: Vec<String> = v["settings"]["global_dial_in_numbers"]
        .as_array()
        .into_iter()
        .flatten()
        .take(4)
        .filter_map(|n| Some(format!("{} ({})", n["number"].as_str()?, n["city"].as_str().or_else(|| n["country_name"].as_str()).unwrap_or(""))))
        .collect();
    if !numbers.is_empty() {
        html.push_str(&format!("<p><b>Join by phone</b><br>{}</p>", numbers.iter().map(|n| esc(n)).collect::<Vec<_>>().join("<br>")));
    }
    Conference { kind: "zoom".into(), link: Some(link), html }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zooms_answer_reads_as_a_meeting() {
        let v = json!({ "id": 85512345678i64, "join_url": "https://zoom.us/j/85512345678?pwd=x", "password": "a1b2", "settings": { "global_dial_in_numbers": [{ "number": "+1 646 558 8656", "city": "New York" }] } });
        let c = conference_of(&v);
        assert_eq!(c.link.as_deref(), Some("https://zoom.us/j/85512345678?pwd=x"));
        assert!(c.html.contains("Meeting ID: 855 1234 5678") && c.html.contains("Passcode: a1b2") && c.html.contains("+1 646 558 8656 (New York)"));
    }
}
