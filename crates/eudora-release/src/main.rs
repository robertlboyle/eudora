//! Signs Eudora releases for its updates (as PlanetPDF's `scripts/release_update.py`; the format
//! is described in `eudora-core/src/update.rs`).
//!
//! Once, the signing key (the private key goes outside the project, to `EUDORA_SIGNING_KEY` or
//! the developer's private folder: never inside the project and never on the server; anyone who
//! has it can sign updates, and losing it means installed copies can't be updated without
//! reinstalling):
//!
//!     cargo run -p eudora-release -- keygen
//!
//! It writes the public half to `crates/eudora-core/update_key.pub`, built into every Eudora.
//! For each release, after `scripts\build-installer.ps1`:
//!
//!     cargo run -p eudora-release -- publish --notes "What's new"
//!
//! That makes `dist\update-site\`: the Setup program, `latest-windows.json` and its `.sig`.
//! Upload them to the update server's folder (Setup first, the .json and .sig last), once the
//! release is approved.
//!
//! (It doesn't use eudora-core, which can't be built before the public key exists.)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use sha2::{Digest, Sha256};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("the project folder")
}

fn public_key_file() -> PathBuf {
    root().join("crates/eudora-core/update_key.pub")
}

/// The developer's private folder (`EUDORA_DEV`), as the builds find Font Awesome and Google's
/// client file.
fn private_dev() -> Option<PathBuf> {
    if let Some(dev) = std::env::var_os("EUDORA_DEV") {
        return Some(PathBuf::from(dev));
    }
    None
}

/// `EUDORA_SIGNING_KEY`, else `Eudora-Key/eudora-release/update_signing_key.pem` in the
/// developer's private folder.
fn key_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("EUDORA_SIGNING_KEY") {
        return Ok(PathBuf::from(p));
    }
    let dev = private_dev().context("No signing key: set EUDORA_SIGNING_KEY to the private key's file")?;
    Ok(dev.join("Eudora-Key").join("eudora-release").join("update_signing_key.pem"))
}

fn outside_project(path: &Path) -> Result<()> {
    let full = path.parent().and_then(|p| p.canonicalize().ok()).unwrap_or_default();
    if full.starts_with(root()) {
        bail!("Keep the private key outside the project folder: {}", path.display());
    }
    Ok(())
}

fn read_key(path: &Path) -> Result<ring::signature::Ed25519KeyPair> {
    let pem = std::fs::read_to_string(path).with_context(|| format!("reading the signing key {}", path.display()))?;
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = B64.decode(body.trim()).context("the signing key isn't PEM")?;
    ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der).map_err(|e| anyhow::anyhow!("the signing key can't be read: {e}"))
}

fn public_b64(pair: &ring::signature::Ed25519KeyPair) -> String {
    use ring::signature::KeyPair;
    B64.encode(pair.public_key().as_ref())
}

fn keygen(replace: bool) -> Result<()> {
    let path = key_path()?;
    outside_project(&path).ok();
    if path.exists() && !replace {
        bail!("{} exists already. Copies built with its public key only accept updates signed by it; use --replace only if you really mean to start over.", path.display());
    }
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow::anyhow!("no key made"))?;
    let b64 = B64.encode(pkcs8.as_ref());
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
    std::fs::create_dir_all(path.parent().unwrap())?;
    outside_project(&path)?;
    std::fs::write(&path, format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", lines.join("\n")))?;
    let pair = read_key(&path)?;
    std::fs::write(public_key_file(), format!("{}\n", public_b64(&pair)))?;
    println!("Private key: {}  (never share it; keep it outside the project)\nPublic key:  {}", path.display(), public_key_file().display());
    Ok(())
}

fn workspace_version() -> Result<String> {
    let toml = std::fs::read_to_string(root().join("Cargo.toml"))?;
    toml.lines()
        .find_map(|l| l.trim().strip_prefix("version").map(|r| r.trim_start_matches([' ', '=']).trim().trim_matches('"').to_string()))
        .context("no version in Cargo.toml")
}

fn publish(args: &[String]) -> Result<()> {
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let version = opt("--version").map(Ok).unwrap_or_else(workspace_version)?;
    let platform = opt("--platform").unwrap_or_else(|| "windows".into());
    let file = opt("--file").map(PathBuf::from).unwrap_or_else(|| root().join("dist").join(format!("Eudora-Setup-{version}.exe")));
    let out = opt("--out").map(PathBuf::from).unwrap_or_else(|| root().join("dist").join("update-site"));
    let notes = opt("--notes").unwrap_or_default();
    if !file.is_file() {
        bail!("{} isn't there: build it first (scripts\\build-installer.ps1), or name it with --file", file.display());
    }
    let pair = read_key(&key_path()?)?;
    let public = std::fs::read_to_string(public_key_file())?;
    if public.trim() != public_b64(&pair) {
        bail!("This private key doesn't match the public key in Eudora: installed copies would refuse it.");
    }
    let name = file.file_name().unwrap().to_string_lossy().to_string();
    let data = std::fs::read(&file)?;
    std::fs::create_dir_all(&out)?;
    std::fs::write(out.join(&name), &data)?;
    let sha: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "app": "Eudora", "platform": platform, "version": version, "file": name, "size": data.len(),
        "sha256": sha, "notes": notes, "published": chrono::Local::now().format("%Y-%m-%d").to_string(),
    }))?;
    let signature = B64.encode(pair.sign(&manifest).as_ref());
    // (checked as Eudora will)
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, B64.decode(public.trim())?)
        .verify(&manifest, &B64.decode(&signature)?)
        .map_err(|_| anyhow::anyhow!("the signature doesn't check"))?;
    let manifest_name = format!("latest-{platform}.json");
    std::fs::write(out.join(&manifest_name), &manifest)?;
    std::fs::write(out.join(format!("{manifest_name}.sig")), format!("{signature}\n"))?;
    println!("Ready to upload from {}: {name} ({:.1} MB), {manifest_name}, {manifest_name}.sig", out.display(), data.len() as f64 / 1e6);
    Ok(())
}

/// The natural voices for one system (core `voices.rs`): the `eudora-voice` program built for it
/// and the Kokoro model, copied into `<out>/voices/` with a list signed by Eudora's key.
fn voices(args: &[String]) -> Result<()> {
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let platform = opt("--platform").unwrap_or_else(|| "windows".into());
    let version = opt("--version").unwrap_or_else(|| workspace_version().unwrap_or_else(|_| "1".into()));
    let windows = platform.starts_with("windows");
    let program = opt("--program").map(PathBuf::from).unwrap_or_else(|| root().join("target").join("release").join(if windows { "eudora-voice.exe" } else { "eudora-voice" }));
    let models = opt("--models").map(PathBuf::from).context("--models DIR: the folder with kokoro-v1.0.onnx and voices-v1.0.bin")?;
    let out = opt("--out").map(PathBuf::from).unwrap_or_else(|| root().join("dist").join("update-site")).join("voices");
    let pair = read_key(&key_path()?)?;
    let public = std::fs::read_to_string(public_key_file())?;
    if public.trim() != public_b64(&pair) {
        bail!("This private key doesn't match the public key in Eudora: installed copies would refuse it.");
    }
    std::fs::create_dir_all(&out)?;
    let program_name = if windows { "eudora-voice.exe" } else { "eudora-voice" };
    let server_program = format!("eudora-voice-{version}-{platform}{}", if windows { ".exe" } else { "" });
    let mut files = vec![(program_name.to_string(), server_program.clone(), program.clone())];
    for name in ["kokoro-v1.0.onnx", "voices-v1.0.bin"] {
        files.push((name.to_string(), name.to_string(), models.join(name)));
    }
    let mut listed = Vec::new();
    for (name, path, from) in &files {
        if !from.is_file() {
            bail!("{} isn't there", from.display());
        }
        let data = std::fs::read(from)?;
        let target = out.join(path);
        if std::fs::read(&target).ok().as_deref() != Some(&data[..]) {
            std::fs::write(&target, &data)?;
        }
        let sha: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
        listed.push(serde_json::json!({ "name": name, "path": path, "size": data.len(), "sha256": sha }));
    }
    let list = serde_json::to_vec_pretty(&serde_json::json!({ "app": "Eudora", "kind": "voices", "platform": platform, "version": version, "files": listed }))?;
    let signature = B64.encode(pair.sign(&list).as_ref());
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, B64.decode(public.trim())?)
        .verify(&list, &B64.decode(&signature)?)
        .map_err(|_| anyhow::anyhow!("the signature doesn't check"))?;
    let list_name = format!("voices-{platform}.json");
    std::fs::write(out.join(&list_name), &list)?;
    std::fs::write(out.join(format!("{list_name}.sig")), format!("{signature}\n"))?;
    let total: u64 = files.iter().map(|(_, _, f)| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)).sum();
    println!("Ready to upload from {}: {server_program}, the model files, {list_name} and .sig ({:.0} MB in all; the list and .sig last)", out.display(), total as f64 / 1e6);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("keygen") => keygen(args.iter().any(|a| a == "--replace")),
        Some("publish") => publish(&args[1..]),
        Some("voices") => voices(&args[1..]),
        _ => {
            println!("eudora-release keygen [--replace]\neudora-release publish [--notes TEXT] [--file SETUP.exe] [--version V] [--platform windows] [--out DIR]
eudora-release voices --models DIR [--program eudora-voice.exe] [--platform windows] [--version V] [--out DIR]");
            Ok(())
        }
    }
}
