//! Twos (twosapp.com): adding a line to the user's "today" list. Twos documents no public API;
//! this is the address its own apps call, with the user id and token the user copies from
//! Twos in a browser.
//! The field names are Twos' as seen in its apps' requests; the Test in Settings checks them.

use crate::error::{Error, Result};

const ADD_TO_TODAY: &str = "https://www.twosapp.com/apiV2/user/addToToday";

/// Adds `text` to the user's Twos "today" list.
pub fn add_to_today(user: &str, token: &str, text: &str) -> Result<()> {
    let agent = crate::graph::agent();
    let body = serde_json::json!({ "text": text, "user_id": user, "token": token }).to_string();
    let mut response = agent.post(ADD_TO_TODAY).header("Content-Type", "application/json").send(body.as_bytes()).map_err(crate::graph::net)?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let answer = response.body_mut().read_to_string().unwrap_or_default();
    Err(Error::Other(format!(
        "Twos answered {status}{}: check your user id and token (Settings ▸ To Do ▸ Twos); Twos may also have changed its unofficial API",
        if answer.trim().is_empty() { String::new() } else { format!(" ({})", answer.chars().take(160).collect::<String>()) }
    )))
}
