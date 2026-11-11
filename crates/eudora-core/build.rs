//! Builds in Eudora's Google OAuth client (the Gmail API, Google contacts and calendars): the
//! JSON Google Cloud gives for a "Desktop app" client (`client_secret_….json`), named by
//! `EUDORA_GOOGLE_CLIENT`, else the newest in the developer's private folder's `Google`
//! (`EUDORA_DEV`). Never in the repository: it's written into the
//! build's own folder. (Google says a desktop app's client secret isn't a secret: it ships inside
//! the program; still, it stays out of the source.) Without it, Google sign-in says it isn't set up.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=EUDORA_GOOGLE_CLIENT");
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let file = std::env::var_os("EUDORA_GOOGLE_CLIENT").map(PathBuf::from).or_else(|| google_dir(&here).and_then(|dir| client_file(&dir)));
    let client = file.as_ref().and_then(|f| {
        println!("cargo:rerun-if-changed={}", f.display());
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(f).ok()?).ok()?;
        let c = if json["installed"].is_object() { &json["installed"] } else { &json["web"] };
        Some((c["client_id"].as_str()?.to_string(), c["client_secret"].as_str().unwrap_or("").to_string()))
    });
    if file.is_some() && client.is_none() {
        println!("cargo:warning=The Google client file isn't Google's OAuth client JSON: Google sign-in left out");
    }
    let code = match &client {
        Some((id, secret)) => format!("pub const GOOGLE_CLIENT: Option<(&str, &str)> = Some(({id:?}, {secret:?}));\n"),
        None => "pub const GOOGLE_CLIENT: Option<(&str, &str)> = None;\n".to_string(),
    };
    // the iOS client (iPhone sign-in: no secret, its own return address): the `.plist` Google
    // Cloud gives for an "iOS" OAuth client, beside the desktop one, or `EUDORA_GOOGLE_IOS_CLIENT`
    println!("cargo:rerun-if-env-changed=EUDORA_GOOGLE_IOS_CLIENT");
    let ios_file = std::env::var_os("EUDORA_GOOGLE_IOS_CLIENT").map(PathBuf::from).or_else(|| google_dir(&here).and_then(|dir| newest(&dir, "plist")));
    let ios = ios_file.as_ref().and_then(|f| {
        println!("cargo:rerun-if-changed={}", f.display());
        plist_string(&std::fs::read_to_string(f).ok()?, "CLIENT_ID")
    });
    if ios_file.is_some() && ios.is_none() {
        println!("cargo:warning=The Google iOS client file has no CLIENT_ID: Google sign-in on iOS left out");
    }
    let code = code + &format!("#[allow(dead_code)]\npub const GOOGLE_IOS_CLIENT: Option<&str> = {};\n", match &ios { Some(id) => format!("Some({id:?})"), None => "None".into() });
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("google_client.rs");
    if std::fs::read_to_string(&out).ok().as_deref() != Some(code.as_str()) {
        std::fs::write(&out, code).unwrap();
    }
}

/// The newest `.json` in the folder holding an OAuth client.
fn client_file(dir: &Path) -> Option<PathBuf> {
    newest(dir, "json")
}

/// A `<key>name</key><string>value</string>` pair's value in a property list.
fn plist_string(text: &str, name: &str) -> Option<String> {
    let after = &text[text.find(&format!("<key>{name}</key>"))?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")? + start;
    Some(after[start..end].trim().to_string()).filter(|v| !v.is_empty())
}

/// The newest file in the folder with this extension.
fn newest(dir: &Path, extension: &str) -> Option<PathBuf> {
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(extension)))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    files.sort();
    files.pop().map(|(_, p)| p)
}

/// The developer's private folder's `Google` (as `eudora-app/build.rs` finds `fontawesome`).
fn google_dir(here: &Path) -> Option<PathBuf> {
    println!("cargo:rerun-if-env-changed=EUDORA_DEV");
    let mut found: Vec<PathBuf> = Vec::new();
    if let Some(dev) = std::env::var_os("EUDORA_DEV") {
        found.push(PathBuf::from(dev));
    }
    let _ = here;
    // (the Dev folders watched too: a Google folder made there later is noticed by the next
    // build; 2026-10-26 the key was put there after a build and was missed)
    for d in found.iter().filter(|d| d.is_dir()) {
        println!("cargo:rerun-if-changed={}", d.display());
    }
    found.into_iter().map(|d| d.join("Google")).find(|d| d.is_dir())
}
