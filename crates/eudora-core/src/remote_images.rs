//! Pictures from the web in HTML mail: found in the message, fetched by Eudora itself (the
//! message's sandbox never goes to the network), handed back as `data:` URLs to put in place.
//! Kept on this computer (`<library>/images/`) when the user's setting says so, else fetched
//! each time. Loading them can tell the sender the message was read: the user decides.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use rayon::prelude::*;
use serde::Serialize;

/// Biggest picture fetched.
const MAX_BYTES: u64 = 15 * 1024 * 1024;
/// Pictures fetched for one message, at most.
const MAX_PICTURES: usize = 200;

/// The web pictures a message's HTML names: each URL as written in the HTML (so it can be
/// replaced there), in order, once.
pub fn find(html: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let lower = html.to_ascii_lowercase();
    // attributes that load pictures, and CSS url(...)
    for marker in ["src=", "background=", "url("] {
        let mut from = 0;
        while let Some(at) = lower[from..].find(marker) {
            let start = from + at + marker.len();
            from = start;
            let rest = &html[start..];
            let rest_trim = rest.trim_start();
            let (quote, body) = match rest_trim.chars().next() {
                Some(q @ ('"' | '\'')) => (Some(q), &rest_trim[1..]),
                _ => (None, rest_trim),
            };
            let end = match quote {
                Some(q) => body.find(q),
                None => body.find(|c: char| c.is_whitespace() || c == '>' || c == ')'),
            }
            .unwrap_or(body.len());
            let url = body[..end].trim();
            let l = url.to_ascii_lowercase();
            if (l.starts_with("http://") || l.starts_with("https://") || l.starts_with("//")) && !found.iter().any(|f| f == url) {
                found.push(url.to_string());
                if found.len() >= MAX_PICTURES {
                    return found;
                }
            }
        }
    }
    found
}

/// One picture: the URL as written in the HTML, and what to put there (None: couldn't load).
#[derive(Clone, Debug, Serialize)]
pub struct Picture {
    pub url: String,
    pub data: Option<String>,
}

/// The URL to request: HTML entities decoded, protocol-relative made https.
fn request_url(written: &str) -> String {
    let url = written.replace("&amp;", "&").replace("&#38;", "&");
    if url.starts_with("//") { format!("https:{url}") } else { url }
}

/// Fetches `urls` (as written in the HTML), several at a time. `keep`: the folder pictures are
/// kept in (used when already there, stored when fetched); None fetches them every time.
pub fn fetch(urls: &[String], keep: Option<&Path>) -> Vec<Picture> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent("Mozilla/5.0 (Eudora)")
        .build();
    let agent = ureq::Agent::new_with_config(config);
    urls.par_iter()
        .map(|written| {
            let url = request_url(written);
            let data = cached(keep, &url).or_else(|| download(&agent, &url).inspect(|(mime, bytes)| store(keep, &url, mime, bytes)));
            Picture { url: written.clone(), data: data.map(|(mime, bytes)| format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))) }
        })
        .collect()
}

fn download(agent: &ureq::Agent, url: &str) -> Option<(String, Vec<u8>)> {
    let mut response = agent.get(url).call().ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .filter(|m| m.starts_with("image/"))
        .unwrap_or_else(|| guess(url).to_string());
    let mut bytes = Vec::new();
    response.body_mut().as_reader().take(MAX_BYTES).read_to_end(&mut bytes).ok()?;
    (!bytes.is_empty()).then_some((mime, bytes))
}

fn guess(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    match path.rsplit('.').next() {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => "image/jpeg",
    }
}

fn cache_path(dir: &Path, url: &str) -> PathBuf {
    let hex = blake3::hash(url.as_bytes()).to_hex();
    dir.join(&hex[..2]).join(&hex[..])
}

fn cached(keep: Option<&Path>, url: &str) -> Option<(String, Vec<u8>)> {
    let path = cache_path(keep?, url);
    let mime = std::fs::read_to_string(path.with_extension("type")).ok()?;
    Some((mime.trim().to_string(), std::fs::read(&path).ok()?))
}

fn store(keep: Option<&Path>, url: &str, mime: &str, bytes: &[u8]) {
    let Some(dir) = keep else { return };
    let path = cache_path(dir, url);
    if std::fs::create_dir_all(path.parent().unwrap()).is_ok() && std::fs::write(&path, bytes).is_ok() {
        let _ = std::fs::write(path.with_extension("type"), mime);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_web_pictures_once_and_leaves_the_rest() {
        let html = r#"<img src="https://a.example/x.png?a=1&amp;b=2"><img src='cid:pic1'>
            <td background=http://b.example/bg.jpg><div style="background:url('//c.example/c.gif')">
            <img SRC="https://a.example/x.png?a=1&amp;b=2"><img src="data:image/png;base64,AAA">"#;
        assert_eq!(find(html), vec!["https://a.example/x.png?a=1&amp;b=2", "http://b.example/bg.jpg", "//c.example/c.gif"]);
        assert_eq!(request_url("https://a.example/x.png?a=1&amp;b=2"), "https://a.example/x.png?a=1&b=2");
        assert_eq!(request_url("//c.example/c.gif"), "https://c.example/c.gif");
    }

    #[test]
    fn kept_pictures_come_from_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        store(Some(dir.path()), "https://a.example/x.png", "image/png", b"PNG");
        // (no network needed: it's already kept)
        let got = fetch(&["https://a.example/x.png".to_string()], Some(dir.path()));
        assert_eq!(got[0].data.as_deref(), Some("data:image/png;base64,UE5H"));
    }
}
