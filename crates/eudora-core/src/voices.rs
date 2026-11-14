//! Natural voices: Eudora's Kokoro speech program (`eudora-voice`) and its model, downloaded only
//! when the user asks, as PlanetPDF downloads its voices.
//!
//! The server keeps, beside Eudora's updates (`<update url>/voices/`), one signed list per system:
//! `voices-<platform>.json` + `.sig` (signed with Eudora's update key, so no one else's files are
//! ever run), naming each file, its size, and its SHA-256:
//!
//! ```json
//! {"app": "Eudora", "kind": "voices", "platform": "windows", "version": "1",
//!  "files": [{"name": "eudora-voice.exe", "path": "eudora-voice-1-windows.exe", "size": …, "sha256": "…"},
//!            {"name": "kokoro-v1.0.onnx", "path": "kokoro-v1.0.onnx", …}, …]}
//! ```
//!
//! Files go into `<Eudora's folder>/voices` (outside the program, so updates keep them, and
//! outside the library, so backups don't carry 340 MB of model), each as `.part` until its size
//! and SHA-256 match; the list and signature are kept beside them and checked again whenever the
//! voices start.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::update::{hex, join, open, public_key, read_small};
use crate::{Error, Result};

/// One file of the voices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceFile {
    /// Its name here.
    pub name: String,
    /// Where it is on the server, under `voices/`.
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// The signed list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoicesList {
    pub version: String,
    pub files: Vec<VoiceFile>,
}

impl VoicesList {
    pub fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// The program's name on this system.
pub fn program_name() -> &'static str {
    if cfg!(windows) { "eudora-voice.exe" } else { "eudora-voice" }
}

fn list_name() -> String {
    format!("voices-{}.json", crate::update::base_platform())
}

/// Where the voices are kept: `<Eudora's folder>/voices`.
pub fn folder() -> Option<PathBuf> {
    crate::library::app_data_dir().map(|d| d.join("voices"))
}

fn safe(name: &str, path_too: bool) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c) || (path_too && c == '/'))
}

/// The list, if Eudora's key signed it and it's for this system.
pub fn verify_with(list: &[u8], signature: &[u8], key: &[u8], platform: &str) -> Result<VoicesList> {
    use base64::Engine;
    let sig = base64::engine::general_purpose::STANDARD.decode(String::from_utf8_lossy(signature).trim()).unwrap_or_default();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(list, &sig)
        .map_err(|_| Error::Other("The natural voices on the server aren't signed with Eudora's key, so they weren't used.".into()))?;
    let unreadable = |why: String| Error::Other(format!("The natural voices' list on the server can't be read ({why})."));
    let data: serde_json::Value = serde_json::from_slice(list).map_err(|e| unreadable(e.to_string()))?;
    if data["app"] != "Eudora" || data["kind"] != "voices" {
        return Err(unreadable("it isn't Eudora's voices".into()));
    }
    if data["platform"].as_str() != Some(platform) {
        return Err(unreadable(format!("it's for {}, not {platform}", data["platform"])));
    }
    let list: VoicesList = serde_json::from_value(data).map_err(|e| unreadable(e.to_string()))?;
    if list.files.is_empty() || !list.files.iter().any(|f| f.name == program_name()) {
        return Err(unreadable("it has no voice program".into()));
    }
    for f in &list.files {
        if !safe(&f.name, false) || !safe(&f.path, true) {
            return Err(unreadable(format!("odd file name {:?}", f.name)));
        }
        if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(unreadable("no SHA-256".into()));
        }
    }
    Ok(VoicesList { files: list.files.into_iter().map(|f| VoiceFile { sha256: f.sha256.to_ascii_lowercase(), ..f }).collect(), ..list })
}

pub fn verify(list: &[u8], signature: &[u8]) -> Result<VoicesList> {
    verify_with(list, signature, &public_key()?, crate::update::base_platform())
}

/// What's on the server for this system (its list and signature, checked).
pub fn available(base: &str) -> Result<(VoicesList, Vec<u8>, Vec<u8>)> {
    let name = list_name();
    let list = read_small(&join(base, &format!("voices/{name}")), 1 << 20).map_err(|e| match e {
        Error::Other(m) if m.contains("HTTP") => Error::Other("The natural voices aren't on the server for this system yet.".into()),
        e => e,
    })?;
    let signature = read_small(&join(base, &format!("voices/{name}.sig")), 4096)?;
    let parsed = verify(&list, &signature)?;
    Ok((parsed, list, signature))
}

/// The voices here, if they're complete: the kept list still verifies and every file is there
/// at its size (the checksums were checked as they came).
pub fn installed() -> Option<(VoicesList, PathBuf)> {
    let dir = folder()?;
    let name = list_name();
    let list = verify(&std::fs::read(dir.join(&name)).ok()?, &std::fs::read(dir.join(format!("{name}.sig"))).ok()?).ok()?;
    let complete = list.files.iter().all(|f| std::fs::metadata(dir.join(&f.name)).is_ok_and(|m| m.len() == f.size));
    complete.then_some((list, dir))
}

fn file_matches(path: &Path, f: &VoiceFile) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if meta.len() != f.size {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut digest = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => digest.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    hex(&digest.finalize()) == f.sha256
}

/// Downloads the voices (`progress(done, total, file)` in bytes over every file; false stops it).
/// Files already here and matching are kept, so a stopped download continues where it was.
pub fn download(base: &str, mut progress: impl FnMut(u64, u64, &str) -> bool) -> Result<PathBuf> {
    let (list, list_bytes, signature) = available(base)?;
    let dir = folder().ok_or_else(|| Error::Other("Eudora has no folder of its own here".into()))?;
    let io = |context: String| move |source| Error::Io { context, source };
    std::fs::create_dir_all(&dir).map_err(io(format!("making {}", dir.display())))?;
    let total = list.total();
    let mut before = 0u64;
    for f in &list.files {
        let target = dir.join(&f.name);
        if !file_matches(&target, f) {
            let part = dir.join(format!("{}.part", f.name));
            let result = (|| -> Result<()> {
                let mut body = open(&join(base, &format!("voices/{}", f.path)))?.into_reader();
                let mut out = std::fs::File::create(&part).map_err(io(format!("writing {}", part.display())))?;
                let mut digest = Sha256::new();
                let mut buf = vec![0u8; 256 * 1024];
                let mut done = 0u64;
                loop {
                    let n = body.read(&mut buf).map_err(|e| Error::Network(format!("The download didn't finish ({e}).")))?;
                    if n == 0 {
                        break;
                    }
                    done += n as u64;
                    if done > f.size {
                        return Err(Error::Other(format!("{} is larger than its list says: it was thrown away.", f.name)));
                    }
                    digest.update(&buf[..n]);
                    out.write_all(&buf[..n]).map_err(io(format!("writing {}", part.display())))?;
                    if !progress(before + done, total, &f.name) {
                        return Err(Error::Canceled);
                    }
                }
                out.sync_all().map_err(io(format!("writing {}", part.display())))?;
                if done != f.size || hex(&digest.finalize()) != f.sha256 {
                    return Err(Error::Other(format!("{} doesn't match its checksum: it was thrown away.", f.name)));
                }
                Ok(())
            })();
            if let Err(e) = result {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
            let _ = std::fs::remove_file(&target);
            std::fs::rename(&part, &target).map_err(io(format!("keeping {}", target.display())))?;
            #[cfg(unix)]
            if f.name == program_name() {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755));
            }
        }
        before += f.size;
        progress(before, total, &f.name);
    }
    // the list last: the voices count as here only once every file is
    let name = list_name();
    std::fs::write(dir.join(format!("{name}.sig")), &signature).map_err(io(format!("writing {name}.sig")))?;
    std::fs::write(dir.join(&name), &list_bytes).map_err(io(format!("writing {name}")))?;
    // files of older voices go
    let keep: Vec<String> = list.files.iter().map(|f| f.name.clone()).chain([name.clone(), format!("{name}.sig")]).collect();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if !keep.contains(&e.file_name().to_string_lossy().to_string()) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(dir)
}

/// Whether the server has other voices than the ones downloaded here (a newer list: another
/// version, or a file whose checksum differs), so `download` should run again; None when none
/// are downloaded here or nothing changed. Found 2026-11-14: the first Windows program made
/// silence, and computers that had it never looked again.
pub fn newer_available(base: &str) -> Result<Option<VoicesList>> {
    let Some((here, _)) = installed() else { return Ok(None) };
    let (there, _, _) = available(base)?;
    let changed = there.version != here.version
        || there.files.len() != here.files.len()
        || there.files.iter().any(|f| !here.files.iter().any(|h| h.name == f.name && h.sha256 == f.sha256 && h.size == f.size));
    Ok(changed.then_some(there))
}

/// How many bytes downloading `there` would fetch, given what's here (only the files that
/// changed: an update of the program alone leaves the 310 MB model where it is).
pub fn to_fetch(there: &VoicesList) -> u64 {
    let Some(dir) = folder() else { return there.total() };
    there.files.iter().filter(|f| !file_matches(&dir.join(&f.name), f)).map(|f| f.size).sum()
}

/// Removes the voices from this computer.
pub fn remove() -> Result<()> {
    if let Some(dir) = folder() {
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|source| Error::Io { context: format!("removing {}", dir.display()), source })?;
        }
    }
    Ok(())
}

/// A signed list for the release tool (`eudora-release voices`): the files in `dir` as `files`
/// names them (name, path on the server), with their sizes and SHA-256s.
pub fn make_list(platform: &str, version: &str, dir: &Path, files: &[(String, String)]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (name, path) in files {
        let file = dir.join(name);
        let size = std::fs::metadata(&file).map_err(|source| Error::Io { context: format!("reading {}", file.display()), source })?.len();
        let f = VoiceFile { name: name.clone(), path: path.clone(), size, sha256: String::new() };
        let mut digest = Sha256::new();
        let mut input = std::fs::File::open(&file).map_err(|source| Error::Io { context: format!("reading {}", file.display()), source })?;
        std::io::copy(&mut input, &mut DigestWriter(&mut digest)).map_err(|source| Error::Io { context: format!("reading {}", file.display()), source })?;
        out.push(VoiceFile { sha256: hex(&digest.finalize()), ..f });
    }
    let value = serde_json::json!({ "app": "Eudora", "kind": "voices", "platform": platform, "version": version, "files": out });
    Ok(serde_json::to_vec_pretty(&value)?)
}

struct DigestWriter<'a>(&'a mut Sha256);
impl Write for DigestWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_list_signed_with_the_key_for_this_system_is_used() {
        use base64::Engine;
        use ring::signature::KeyPair;
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let key = pair.public_key().as_ref().to_vec();
        let sign = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(pair.sign(b).as_ref()).into_bytes();
        let sha = "a".repeat(64);
        let list = |name: &str, platform: &str| {
            serde_json::to_vec(&serde_json::json!({ "app": "Eudora", "kind": "voices", "platform": platform, "version": "1",
                "files": [{ "name": program_name(), "path": "p/x", "size": 1, "sha256": sha }, { "name": name, "path": "m", "size": 2, "sha256": sha }] }))
            .unwrap()
        };
        let good = list("kokoro-v1.0.onnx", "windows");
        assert_eq!(verify_with(&good, &sign(&good), &key, "windows").unwrap().total(), 3);
        assert!(verify_with(&good, &sign(&good), &key, "macos").is_err(), "another system's");
        let mut changed = good.clone();
        changed[10] ^= 1;
        assert!(verify_with(&changed, &sign(&good), &key, "windows").is_err(), "changed after signing");
        let sneaky = list("../evil.exe", "windows");
        assert!(verify_with(&sneaky, &sign(&sneaky), &key, "windows").is_err(), "a name leaving the folder");
    }
}
