//! New appointments.
//!
//! Places: Google Maps' suggestions with the user's own Google Maps key (Places API (New)
//! Autocomplete; the key in the credential store, billed to its owner), else OpenStreetMap's
//! through Photon (no key). Online meetings: Teams = the event made with "online meeting" on in a
//! Microsoft 365 calendar (Microsoft adds it to the invitation); Google Meet asked of a signed-in
//! Google account's calendar (an event there holds it), its link and dial-in put in the event;
//! Zoom = the user's Personal Meeting Room from Settings (setting `zoom-room`). The event goes
//! to Microsoft 365 with its people: Microsoft sends the invitations (`Library::create_event`).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::library::Library;

/// The Google Maps key's name in the credential store.
const MAPS_KEY: &str = "maps:google";
/// The Zoom room (setting): {link, passcode, dial}.
pub const ZOOM_ROOM: &str = "zoom-room";

/// A place suggested for a meeting's location.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Place {
    /// "Starbucks", "Newark Liberty International Airport" (or the address itself).
    pub name: String,
    /// "123 Main St, Springfield, IL 62701, USA"
    pub address: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// A map of it (Google Maps or OpenStreetMap).
    pub map_url: String,
    /// "google" or "openstreetmap".
    pub source: String,
}

/// The user's Google Maps key, when one is set.
pub fn maps_key() -> Option<String> {
    crate::graph::load_secret(MAPS_KEY).and_then(|b| String::from_utf8(b).ok()).filter(|k| !k.trim().is_empty())
}

/// Sets (or, with None / empty, removes) the Google Maps key.
pub fn set_maps_key(key: Option<&str>) -> Result<()> {
    match key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => crate::graph::store_secret(MAPS_KEY, k.as_bytes()),
        None => {
            crate::graph::delete_secret(MAPS_KEY);
            Ok(())
        }
    }
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(8))).user_agent("Eudora").http_status_as_error(false).build();
    ureq::Agent::new_with_config(config)
}

fn encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// Places matching what's typed: Google's with a key, else OpenStreetMap's.
pub fn suggest_places(text: &str) -> Result<Vec<Place>> {
    let text = text.trim();
    if text.chars().count() < 3 {
        return Ok(Vec::new());
    }
    let mut found = match maps_key() {
        Some(key) => google_places(text, &key)?,
        None => osm_places(text)?,
    };
    // (OpenStreetMap lists an airport once per terminal: each place once)
    let mut seen = std::collections::HashSet::new();
    found.retain(|p| seen.insert((p.name.to_lowercase(), p.address.to_lowercase())));
    Ok(found)
}

/// Google Maps (Places API (New) Autocomplete).
fn google_places(text: &str, key: &str) -> Result<Vec<Place>> {
    let mut response = agent()
        .post("https://places.googleapis.com/v1/places:autocomplete")
        .header("X-Goog-Api-Key", key)
        .header("Content-Type", "application/json")
        .send(json!({ "input": text }).to_string().as_bytes())
        .map_err(|e| Error::Network(format!("Google Maps: {e}")))?;
    let status = response.status().as_u16();
    let body: Value = serde_json::from_str(&response.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
    if status != 200 {
        let why = body["error"]["message"].as_str().unwrap_or("no answer");
        return Err(Error::Other(format!("Google Maps refused the search ({status}): {why}")));
    }
    Ok(body["suggestions"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|s| {
                    let p = &s["placePrediction"];
                    let id = p["placeId"].as_str()?;
                    let whole = p["text"]["text"].as_str().unwrap_or_default();
                    let name = p["structuredFormat"]["mainText"]["text"].as_str().unwrap_or(whole).to_string();
                    let address = p["structuredFormat"]["secondaryText"]["text"].as_str().unwrap_or_default().to_string();
                    Some(Place {
                        map_url: format!("https://www.google.com/maps/search/?api=1&query={}&query_place_id={}", encode(whole), encode(id)),
                        name,
                        address,
                        lat: None,
                        lon: None,
                        source: "google".into(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// OpenStreetMap through Photon (Komoot's search; no key).
fn osm_places(text: &str) -> Result<Vec<Place>> {
    let mut response = agent()
        .get(&format!("https://photon.komoot.io/api/?q={}&limit=7&lang=en", encode(text)))
        .call()
        .map_err(|e| Error::Network(format!("OpenStreetMap: {e}")))?;
    if response.status().as_u16() != 200 {
        return Err(Error::Other(format!("OpenStreetMap's search answered {}", response.status())));
    }
    let body: Value = serde_json::from_str(&response.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
    Ok(body["features"].as_array().map(|list| list.iter().filter_map(osm_place).collect()).unwrap_or_default())
}

/// One of Photon's answers as a place.
fn osm_place(f: &Value) -> Option<Place> {
    let p = &f["properties"];
    let s = |k: &str| p[k].as_str().unwrap_or("").trim().to_string();
    let street = [s("housenumber"), s("street")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" ");
    let region = [s("state"), s("postcode")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" ");
    let address = [street, s("city"), region, s("country")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ");
    let name = if s("name").is_empty() { address.clone() } else { s("name") };
    if name.is_empty() {
        return None;
    }
    let lon = f["geometry"]["coordinates"][0].as_f64();
    let lat = f["geometry"]["coordinates"][1].as_f64();
    let map_url = match (lat, lon) {
        (Some(la), Some(lo)) => format!("https://www.openstreetmap.org/?mlat={la}&mlon={lo}#map=17/{la}/{lo}"),
        _ => format!("https://www.openstreetmap.org/search?query={}", encode(&name)),
    };
    Some(Place { name: name.clone(), address: if address == name { String::new() } else { address }, lat, lon, map_url, source: "openstreetmap".into() })
}

/// The user's Zoom room: (link, passcode, dial-in), when set.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoomRoom {
    pub link: String,
    pub passcode: String,
    /// Dial-in numbers and meeting ID, as Zoom gives them (free text).
    pub dial: String,
}

/// In an event change for a Google calendar: Google is to make a Meet with the event.
pub const GOOGLE_MEET: &str = "eudoraGoogleMeet";

/// An online meeting ready to go in an event.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Conference {
    /// "teams", "meet" or "zoom".
    pub kind: String,
    /// The join link (Teams: made by Microsoft when the event is).
    pub link: Option<String>,
    /// What goes in the event's text (HTML): how to join.
    pub html: String,
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The Zoom room as a meeting's details.
pub fn zoom_conference(room: &ZoomRoom) -> Result<Conference> {
    if !room.link.starts_with("https://") {
        return Err(Error::Other("set your Zoom room's link in Settings ▸ Calendar first".into()));
    }
    let mut html = format!("<p><b>Join Zoom meeting</b><br><a href=\"{0}\">{0}</a>", esc(&room.link));
    if !room.passcode.trim().is_empty() {
        html.push_str(&format!("<br>Passcode: {}", esc(room.passcode.trim())));
    }
    html.push_str("</p>");
    if !room.dial.trim().is_empty() {
        html.push_str(&format!("<p><b>Join by phone</b><br>{}</p>", esc(room.dial.trim()).replace('\n', "<br>")));
    }
    Ok(Conference { kind: "zoom".into(), link: Some(room.link.clone()), html })
}

/// A Google Meet, asked of this Google account's calendar: an event there (free, marked as
/// Eudora's) holds it. Its link and dial-in as a meeting's details.
pub fn google_meet(account: &str, subject: &str, start: i64, end: i64) -> Result<Conference> {
    let mut client = crate::google::GoogleClient::resume(account)?;
    let at = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.to_rfc3339()).unwrap_or_default();
    let mut id = [0u8; 12];
    getrandom::fill(&mut id).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    let request: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let made = client.post_json(
        "https://www.googleapis.com/calendar/v3/calendars/primary/events?conferenceDataVersion=1",
        &json!({
            "summary": subject,
            "description": "Made by Eudora to hold the Google Meet of an appointment on another calendar.",
            "start": { "dateTime": at(start) },
            "end": { "dateTime": at(end) },
            "transparency": "transparent",
            "conferenceData": { "createRequest": { "requestId": request, "conferenceSolutionKey": { "type": "hangoutsMeet" } } },
        }),
    )?;
    let points = made["conferenceData"]["entryPoints"].as_array().cloned().unwrap_or_default();
    let video = points.iter().find(|p| p["entryPointType"] == "video").and_then(|p| p["uri"].as_str()).or_else(|| made["hangoutLink"].as_str()).map(str::to_string);
    let Some(link) = video else {
        return Err(Error::Other("Google didn't make the Meet (its calendar answered without a link)".into()));
    };
    let mut html = format!("<p><b>Join with Google Meet</b><br><a href=\"{0}\">{0}</a></p>", esc(&link));
    if let Some(phone) = points.iter().find(|p| p["entryPointType"] == "phone") {
        let number = phone["label"].as_str().or_else(|| phone["uri"].as_str()).unwrap_or("").trim_start_matches("tel:");
        let pin = phone["pin"].as_str().unwrap_or("");
        html.push_str(&format!("<p><b>Join by phone</b><br>{}{}</p>", esc(number), if pin.is_empty() { String::new() } else { format!(" PIN: {}", esc(pin)) }));
    }
    Ok(Conference { kind: "meet".into(), link: Some(link), html })
}

/// A new appointment as the window gives it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct NewEvent {
    pub calendar: i64,
    pub subject: String,
    /// Unix seconds, UTC; an all-day event: midnight UTC of its first day and after its last.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    /// As typed, and the place chosen from the suggestions, if one was.
    pub location: String,
    pub place: Option<Place>,
    /// "teams", "meet", "zoom", or none.
    pub conference: Option<String>,
    /// The Google account a Meet is asked of.
    pub google: Option<String>,
    /// As typed ("Ann <ann@x.com>, bob@y.com").
    pub attendees: String,
    /// The text (HTML).
    pub notes: String,
    pub reminder: Option<i64>,
    /// free, tentative, busy, oof
    pub show_as: String,
    pub private: bool,
}

impl NewEvent {
    /// An event read from a calendar file (or an invitation email's), as the user's own copy:
    /// never with its people (adding it mustn't send them anything); a repeating one, the first.
    pub fn from_ics(calendar: i64, e: &crate::ics::IcsEvent) -> NewEvent {
        let mut notes = esc(&e.description).replace('\n', "<br>");
        if let Some(r) = &e.repeats {
            notes.push_str(&format!("<p>(It repeats: {}. Only the first was added.)</p>", esc(r)));
        }
        NewEvent {
            calendar,
            subject: e.summary.clone(),
            start: e.start,
            end: e.end,
            all_day: e.all_day,
            location: e.location.clone(),
            place: None,
            conference: None,
            google: None,
            attendees: String::new(),
            notes,
            reminder: (!e.all_day).then_some(15),
            show_as: "busy".into(),
            private: false,
        }
    }
}

/// The event as Microsoft 365 takes it (times in UTC), the online meeting's details added.
pub fn graph_event(e: &NewEvent, conference: Option<&Conference>) -> Result<Value> {
    if e.subject.trim().is_empty() {
        return Err(Error::Other("give it a title".into()));
    }
    if e.end <= e.start {
        return Err(Error::Other("it has to end after it starts".into()));
    }
    let iso = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string()).unwrap_or_default();
    let mut attendees = Vec::new();
    for entry in crate::compose::split_addresses(&e.attendees) {
        let address = crate::compose::address_of(&entry);
        if !address.contains('@') {
            return Err(Error::Other(format!("“{entry}” isn't an email address")));
        }
        let name = entry.split('<').next().unwrap_or("").trim().trim_matches('"').trim().to_string();
        attendees.push(json!({ "type": "required", "emailAddress": { "address": address, "name": if name.is_empty() { address.clone() } else { name } } }));
    }
    let mut body = e.notes.clone();
    if let Some(c) = conference.filter(|c| c.kind != "teams") {
        body.push_str(&format!("<hr>{}", c.html));
    }
    let mut location = json!({ "displayName": e.location.trim() });
    match &e.place {
        Some(p) => {
            let shown = if p.address.is_empty() || e.location.trim() != p.name { e.location.trim().to_string() } else { format!("{}, {}", p.name, p.address) };
            location = json!({ "displayName": shown, "locationUri": p.map_url, "address": { "street": p.address } });
            if let (Some(lat), Some(lon)) = (p.lat, p.lon) {
                location["coordinates"] = json!({ "latitude": lat, "longitude": lon });
            }
        }
        None if e.location.trim().is_empty() => {
            if let Some(c) = conference.filter(|c| c.kind != "teams") {
                location = json!({ "displayName": match c.kind.as_str() { "meet" => "Google Meet", _ => "Zoom" } });
            }
        }
        None => {}
    }
    let mut v = json!({
        "subject": e.subject.trim(),
        "start": { "dateTime": iso(e.start), "timeZone": "UTC" },
        "end": { "dateTime": iso(e.end), "timeZone": "UTC" },
        "isAllDay": e.all_day,
        "location": location,
        "body": { "contentType": "html", "content": body },
        "attendees": attendees,
        "showAs": if e.show_as.is_empty() { "busy" } else { e.show_as.as_str() },
        "isReminderOn": e.reminder.is_some(),
        "reminderMinutesBeforeStart": e.reminder.unwrap_or(0),
        "sensitivity": if e.private { "private" } else { "normal" },
    });
    if conference.is_some_and(|c| c.kind == "teams") {
        v["isOnlineMeeting"] = true.into();
        v["onlineMeetingProvider"] = "teamsForBusiness".into();
    }
    Ok(v)
}

/// Which online meetings can be added to an event in this calendar: Teams (a Microsoft 365
/// work account's calendar), Google Meet (a signed-in Google account each), Zoom (a room set).
#[derive(Clone, Debug, Default, Serialize)]
pub struct ConferenceOptions {
    pub teams: bool,
    pub google: Vec<String>,
    pub zoom: bool,
    /// Signed in to Zoom: each appointment gets a meeting of its own (else the room).
    pub zoom_meetings: bool,
}

impl Library {
    pub fn conference_options(&self, calendar: i64) -> Result<ConferenceOptions> {
        let (kind, address): (String, String) = self.conn.query_row(
            "SELECT a.kind, COALESCE(a.address, '') FROM calendars c JOIN accounts a ON a.id = c.account WHERE c.id = ?1",
            rusqlite::params![calendar],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // personal Microsoft accounts (Outlook.com, Hotmail…) have no Teams for business
        let personal = crate::mail_host::is_microsoft_personal(&address);
        Ok(ConferenceOptions { teams: kind == "graph" && !personal, google: crate::google::signed_in_accounts(), zoom: crate::zoom::signed_in() || self.zoom_room()?.is_some(), zoom_meetings: crate::zoom::signed_in() })
    }

    /// Whether a calendar is a Google account's (its events go to Google Calendar).
    pub fn google_calendar(&self, calendar: i64) -> Result<bool> {
        use rusqlite::OptionalExtension;
        let kind: Option<String> = self.conn.query_row("SELECT a.kind FROM calendars c JOIN accounts a ON a.id = c.account WHERE c.id = ?1", rusqlite::params![calendar], |r| r.get(0)).optional()?;
        Ok(kind.as_deref() == Some("gmail"))
    }

    pub fn zoom_room(&self) -> Result<Option<ZoomRoom>> {
        Ok(self.setting(ZOOM_ROOM)?.and_then(|j| serde_json::from_str::<ZoomRoom>(&j).ok()).filter(|r| r.link.starts_with("https://")))
    }

    /// Makes a new appointment: its online meeting first (Meet / Zoom details in its text; Teams
    /// made by Microsoft with it), then the event here at once and on the server through the
    /// queue, where Microsoft sends the invitations. Returns its id here.
    pub fn new_appointment(&self, e: &NewEvent) -> Result<i64> {
        let conference = match e.conference.as_deref() {
            None | Some("") => None,
            Some("teams") => Some(Conference { kind: "teams".into(), ..Default::default() }),
            // a meeting of its own when signed in to Zoom, else the Personal Meeting Room
            Some("zoom") if crate::zoom::signed_in() => Some(crate::zoom::create_meeting(&e.subject, e.start, e.end)?),
            Some("zoom") => Some(zoom_conference(&self.zoom_room()?.unwrap_or_default())?),
            // (an event in a Google calendar holds its own Meet: Google makes it with the event)
            Some("meet") if self.google_calendar(e.calendar)? => Some(Conference { kind: "meet".into(), ..Default::default() }),
            Some("meet") => {
                let account = e.google.clone().or_else(|| crate::google::signed_in_accounts().into_iter().next()).ok_or_else(|| Error::Other("sign in to a Google account first (Settings ▸ Accounts)".into()))?;
                Some(google_meet(&account, &e.subject, e.start, e.end)?)
            }
            Some(other) => return Err(Error::Other(format!("unknown kind of meeting {other}"))),
        };
        let mut graph = graph_event(e, conference.as_ref())?;
        if conference.as_ref().is_some_and(|c| c.kind == "meet" && c.link.is_none()) {
            graph[GOOGLE_MEET] = true.into();
        }
        self.create_event(e.calendar, &graph)
    }
}

/// An event as an iCalendar file (.ics: Outlook, Google Calendar, Apple Calendar and phones read
/// it), to send to anyone. METHOD:PUBLISH: the people who get it add it to their calendars
/// (Microsoft's own invitations stay the way to collect answers). `text`: its description as
/// plain text.
pub fn ics(e: &crate::calendar::Event, text: &str, stamp: i64) -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace("\r\n", "\\n").replace('\n', "\\n")
    }
    // lines of at most 75 octets, continued with a space (RFC 5545 §3.1), never inside a character
    fn fold(line: &str) -> String {
        let mut out = String::new();
        let mut width = 0;
        for c in line.chars() {
            if width + c.len_utf8() > 75 {
                out.push_str("\r\n ");
                width = 1;
            }
            out.push(c);
            width += c.len_utf8();
        }
        out.push_str("\r\n");
        out
    }
    let utc = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y%m%dT%H%M%SZ").to_string()).unwrap_or_default();
    let date = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y%m%d").to_string()).unwrap_or_default();
    let uid = if e.ical_uid.is_empty() { format!("{}-{}@eudora", e.start, e.subject.len()) } else { e.ical_uid.clone() };
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".into(),
        "PRODID:-//Planet Networks//Eudora//EN".into(),
        "METHOD:PUBLISH".into(),
        "BEGIN:VEVENT".into(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{}", utc(stamp)),
    ];
    if e.all_day {
        lines.push(format!("DTSTART;VALUE=DATE:{}", date(e.start)));
        lines.push(format!("DTEND;VALUE=DATE:{}", date(e.end)));
    } else {
        lines.push(format!("DTSTART:{}", utc(e.start)));
        lines.push(format!("DTEND:{}", utc(e.end)));
    }
    lines.push(format!("SUMMARY:{}", esc(&e.subject)));
    if !e.location.is_empty() {
        lines.push(format!("LOCATION:{}", esc(&e.location)));
    }
    let mut description = text.trim().to_string();
    if let Some(join) = &e.join_url {
        description = format!("Join the meeting: {join}\n\n{description}");
        lines.push(format!("URL:{join}"));
    }
    if !description.is_empty() {
        lines.push(format!("DESCRIPTION:{}", esc(description.trim())));
    }
    if !e.organizer.address.is_empty() {
        lines.push(format!("ORGANIZER;CN={}:mailto:{}", esc(if e.organizer.name.is_empty() { &e.organizer.address } else { &e.organizer.name }), e.organizer.address));
    }
    for a in &e.attendees {
        lines.push(format!("ATTENDEE;CN={};ROLE=REQ-PARTICIPANT:mailto:{}", esc(if a.name.is_empty() { &a.address } else { &a.name }), a.address));
    }
    lines.push(format!("TRANSP:{}", if e.show_as == "free" { "TRANSPARENT" } else { "OPAQUE" }));
    lines.extend(["END:VEVENT".to_string(), "END:VCALENDAR".to_string()]);
    lines.iter().map(|l| fold(l)).collect()
}

/// An appointment for its editor: the event, its whole text when known (Microsoft sends Eudora
/// only a preview with the calendar; the window asks for the rest), and where it lives.
#[derive(Clone, Debug, Serialize)]
pub struct Appointment {
    pub id: i64,
    pub calendar: i64,
    pub event: crate::calendar::Event,
    /// Its text (HTML), or None when only the preview is here.
    pub body: Option<String>,
    /// The place's map link, when it has one.
    pub map_url: Option<String>,
    /// Already an online meeting (Teams made with it, kept as it is).
    pub online: bool,
    /// On the server already (not still waiting to be made there).
    pub on_server: bool,
    /// The account's address and the event's id there (to ask for its whole text).
    #[serde(skip)]
    pub address: String,
    #[serde(skip)]
    pub server_id: String,
}

impl Library {
    /// An appointment as its editor shows it.
    pub fn appointment(&self, id: i64) -> Result<Appointment> {
        let (calendar, server_id, json, original, address): (i64, String, String, Option<String>, String) = self.conn.query_row(
            "SELECT e.calendar, e.server_id, e.event, e.original, COALESCE(a.address, '') FROM events e JOIN calendars c ON c.id = e.calendar JOIN accounts a ON a.id = c.account WHERE e.id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let event: crate::calendar::Event = serde_json::from_str(&json).unwrap_or_default();
        let original: Value = original.and_then(|o| serde_json::from_str(&o).ok()).unwrap_or(Value::Null);
        Ok(Appointment {
            id,
            calendar,
            body: original["body"]["content"].as_str().map(str::to_string),
            map_url: original["location"]["locationUri"].as_str().map(str::to_string).filter(|u| u.starts_with("http")),
            online: original["isOnlineMeeting"].as_bool().unwrap_or(false) || event.join_url.is_some(),
            on_server: !server_id.starts_with("local:"),
            event,
            address,
            server_id,
        })
    }

    /// Saves an appointment's changes: only meetings the user organized; here at once,
    /// then on the server, where Microsoft sends the people the update. The text goes only
    /// when `body_changed` (or a meeting is added to it), so a Teams block or formatting
    /// isn't lost.
    pub fn edit_appointment(&self, id: i64, e: &NewEvent, body_changed: bool) -> Result<()> {
        let now = self.appointment(id)?;
        if !now.event.is_organizer {
            return Err(Error::Other("it's someone else's meeting: only its organizer can change it (you can still answer it, show it as, categorize it)".into()));
        }
        // an online meeting added now (one already there stays as it is)
        let conference = match e.conference.as_deref() {
            _ if now.online => None,
            None | Some("") => None,
            Some("teams") => Some(Conference { kind: "teams".into(), ..Default::default() }),
            Some("zoom") if crate::zoom::signed_in() => Some(crate::zoom::create_meeting(&e.subject, e.start, e.end)?),
            Some("zoom") => Some(zoom_conference(&self.zoom_room()?.unwrap_or_default())?),
            // (an event in a Google calendar holds its own Meet: Google makes it with the event)
            Some("meet") if self.google_calendar(now.calendar)? => Some(Conference { kind: "meet".into(), ..Default::default() }),
            Some("meet") => {
                let account = e.google.clone().or_else(|| crate::google::signed_in_accounts().into_iter().next()).ok_or_else(|| Error::Other("sign in to a Google account first (Settings ▸ Accounts)".into()))?;
                Some(google_meet(&account, &e.subject, e.start, e.end)?)
            }
            Some(other) => return Err(Error::Other(format!("unknown kind of meeting {other}"))),
        };
        let mut patch = graph_event(e, conference.as_ref())?;
        if conference.as_ref().is_some_and(|c| c.kind == "meet" && c.link.is_none()) {
            patch[GOOGLE_MEET] = true.into();
        }
        let added_text = conference.as_ref().is_some_and(|c| c.kind != "teams" && !c.html.is_empty());
        if !body_changed && !added_text {
            patch.as_object_mut().map(|o| o.remove("body"));
        }
        if added_text && !body_changed {
            // the meeting's details go under the text there already
            let old = now.body.clone().unwrap_or_default();
            patch["body"] = json!({ "contentType": "html", "content": format!("{old}<hr>{}", conference.as_ref().map(|c| c.html.as_str()).unwrap_or("")) });
        }
        // here at once: the event as Microsoft would give it back
        let (account, server_id, _) = self.event_for_change(id)?;
        let original: String = self.conn.query_row("SELECT COALESCE(original, '{}') FROM events WHERE id = ?1", rusqlite::params![id], |r| r.get(0))?;
        let mut merged: Value = serde_json::from_str(&original).unwrap_or_else(|_| json!({}));
        for (k, v) in patch.as_object().into_iter().flatten() {
            merged[k] = v.clone();
        }
        let mut shown = crate::calendar::from_graph(&merged);
        shown.is_organizer = true;
        shown.response = now.event.response.clone();
        if conference.as_ref().is_some_and(|c| c.kind == "teams") || now.online {
            shown.join_url = now.event.join_url.clone();
        }
        self.in_transaction(|| {
            self.conn.execute(
                "UPDATE events SET start = ?2, end = ?3, all_day = ?4, subject = ?5, location = ?6, event = ?7, original = ?8 WHERE id = ?1",
                rusqlite::params![id, shown.start, shown.end, shown.all_day, shown.subject, shown.location, serde_json::to_string(&shown)?, merged.to_string()],
            )?;
            if server_id.starts_with("local:") {
                // not on the server yet: the change goes with its making
                self.conn.execute(
                    "UPDATE pending_ops SET payload = json_set(payload, '$.patch', json_patch(json_extract(payload, '$.patch'), json(?2)))
                     WHERE kind = 'event-change' AND json_extract(payload, '$.server_id') = ?1 AND json_extract(payload, '$.action') = 'create'",
                    rusqlite::params![server_id, patch.to_string()],
                )?;
                Ok(())
            } else {
                self.queue_event_change(json!({ "event": id, "account": account, "server_id": server_id, "action": "update", "patch": patch }))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_becomes_an_ics_file() {
        let e = crate::calendar::Event {
            subject: "Planning, round 2".into(),
            start: 1_800_000_000,
            end: 1_800_003_600,
            location: "Newark Liberty International Airport, Newark, NJ".into(),
            organizer: crate::calendar::Person { name: "Pat Morgan".into(), address: "pat@example.com".into() },
            attendees: vec![crate::calendar::Attendee { name: "Ann Smith".into(), address: "ann@x.com".into(), kind: "required".into(), response: "none".into() }],
            join_url: Some("https://teams.microsoft.com/l/meetup-join/abc".into()),
            ical_uid: "040000008200E00074C5B7101A82E008".into(),
            show_as: "busy".into(),
            ..Default::default()
        };
        let text = ics(&e, "Agenda: budget; hiring", 1_799_990_000);
        assert!(text.starts_with("BEGIN:VCALENDAR\r\n") && text.ends_with("END:VCALENDAR\r\n"));
        assert!(text.contains("METHOD:PUBLISH") && text.contains("DTSTART:20270115T080000Z") && text.contains("SUMMARY:Planning\\, round 2"));
        assert!(text.contains("ATTENDEE;CN=Ann Smith;ROLE=REQ-PARTICIPANT:mailto:ann@x.com") && text.contains("ORGANIZER;CN=Pat Morgan:mailto:pat@example.com"));
        assert!(text.contains("Agenda: budget\\; hiring"));
        // folded lines stay at 75 octets
        assert!(text.split("\r\n").all(|l| l.len() <= 75), "{text}");
        let all_day = ics(&crate::calendar::Event { all_day: true, start: 1_800_000_000 - 1_800_000_000 % 86_400, end: 1_800_000_000 - 1_800_000_000 % 86_400 + 86_400, ..e }, "", 0);
        assert!(all_day.contains("DTSTART;VALUE=DATE:20270115"));
    }

    #[test]
    fn an_openstreetmap_answer_reads_as_a_place() {
        let f = json!({ "geometry": { "coordinates": [-104.67, 39.85] }, "properties": { "name": "Denver International Airport", "city": "Denver", "state": "Colorado", "country": "United States" } });
        let p = osm_place(&f).unwrap();
        assert_eq!(p.name, "Denver International Airport");
        assert_eq!(p.address, "Denver, Colorado, United States");
        assert_eq!((p.lat, p.lon), (Some(39.85), Some(-104.67)));
        assert!(p.map_url.contains("mlat=39.85"));
        // an address with no name of its own
        let f = json!({ "properties": { "housenumber": "100", "street": "Elm St", "city": "Springfield", "state": "IL", "postcode": "62701" } });
        assert_eq!(osm_place(&f).unwrap().name, "100 Elm St, Springfield, IL 62701");
    }

    #[test]
    fn an_appointment_goes_to_microsoft_with_its_people_place_and_meeting() {
        let e = NewEvent {
            calendar: 1,
            subject: "Planning".into(),
            start: 1_800_000_000,
            end: 1_800_003_600,
            location: "Newark Liberty International Airport".into(),
            place: Some(Place { name: "Newark Liberty International Airport".into(), address: "Newark, NJ".into(), lat: Some(40.69), lon: Some(-74.17), map_url: "https://www.openstreetmap.org/x".into(), source: "openstreetmap".into() }),
            attendees: "Ann Smith <ann@x.com>, bob@y.com".into(),
            notes: "<p>Agenda</p>".into(),
            reminder: Some(15),
            ..Default::default()
        };
        let zoom = zoom_conference(&ZoomRoom { link: "https://zoom.us/j/123".into(), passcode: "42".into(), dial: "+1 555 0100".into() }).unwrap();
        let v = graph_event(&e, Some(&zoom)).unwrap();
        assert_eq!(v["attendees"][0]["emailAddress"]["address"], "ann@x.com");
        assert_eq!(v["attendees"][0]["emailAddress"]["name"], "Ann Smith");
        assert_eq!(v["attendees"][1]["emailAddress"]["address"], "bob@y.com");
        assert_eq!(v["location"]["displayName"], "Newark Liberty International Airport, Newark, NJ");
        assert_eq!(v["location"]["coordinates"]["latitude"], 40.69);
        assert!(v["body"]["content"].as_str().unwrap().contains("https://zoom.us/j/123") && v["body"]["content"].as_str().unwrap().contains("Passcode: 42"));
        assert!(v.get("isOnlineMeeting").is_none());
        let teams = graph_event(&e, Some(&Conference { kind: "teams".into(), ..Default::default() })).unwrap();
        assert_eq!((teams["isOnlineMeeting"].as_bool(), teams["onlineMeetingProvider"].as_str()), (Some(true), Some("teamsForBusiness")));
        assert!(graph_event(&NewEvent { subject: "x".into(), start: 10, end: 5, ..Default::default() }, None).is_err());
        assert!(graph_event(&NewEvent { subject: "x".into(), start: 1, end: 5, attendees: "not an address".into(), ..Default::default() }, None).is_err());
        assert!(zoom_conference(&ZoomRoom::default()).is_err(), "no room set");
    }
}
