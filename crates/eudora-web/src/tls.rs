//! HTTPS. Until a real certificate is set up (Let's Encrypt later, or `--cert` / `--key`),
//! Eudora makes its own: a small certificate authority of its own ("Eudora local CA",
//! 10 years) that signs the server's certificate (397 days, the longest phones accept, made
//! again when a month is left or this computer's names / addresses change). Install the CA on
//! a phone once (it's at /ca.crt) and every later server certificate is trusted without warnings.
//!
//! Files in `<library>/web/tls/`: `ca.key.pem`, `ca.pem`, `server.key.pem`, `server.pem`,
//! `server.json` (when it runs out, which names it covers).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Datelike, Duration, Utc};
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, date_time_ymd};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, String>;

const CA_NAME: &str = "Eudora local CA";
const SERVER_DAYS: i64 = 397;
const RENEW_DAYS: i64 = 30;

#[derive(Serialize, Deserialize, PartialEq)]
struct ServerInfo {
    /// Unix seconds.
    not_after: i64,
    names: Vec<String>,
}

fn ca_params() -> CertificateParams {
    let mut p = CertificateParams::default();
    p.distinguished_name.push(DnType::CommonName, CA_NAME);
    p.distinguished_name.push(DnType::OrganizationName, "Eudora on this computer");
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    p
}

/// The names this computer answers to: localhost, its own name, its addresses.
pub fn local_names(extra: &[String]) -> Vec<String> {
    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string(), "::1".to_string()];
    if let Ok(host) = std::env::var("COMPUTERNAME") {
        names.push(host.to_lowercase());
    }
    // the address other computers reach it at (no packet is sent: a UDP "connect" only picks a route)
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("192.0.2.1:9").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                names.push(addr.ip().to_string());
            }
        }
    }
    names.extend(extra.iter().map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty()));
    names.sort();
    names.dedup();
    names
}

fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

/// Makes the CA (once) and a server certificate for `names` when there's none, it runs out
/// within a month, or the names changed. Returns the folder.
pub fn ensure_self_signed(library: &Path, names: &[String]) -> Result<PathBuf> {
    let ymd = |when: chrono::DateTime<Utc>| date_time_ymd(when.year(), when.month() as u8, when.day() as u8);
    let dir = library.join("web").join("tls");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let (ca_key_path, ca_path) = (dir.join("ca.key.pem"), dir.join("ca.pem"));
    // (a CA made under another name, "Eudora 27 local CA" before 1.0.0, is made again: what it
    // signs must name it as it is)
    let named = std::fs::read_to_string(dir.join("ca-name.txt")).ok();
    let new_ca = !ca_key_path.exists() || !ca_path.exists() || named.as_deref() != Some(CA_NAME);
    let ca_key = if new_ca {
        let key = KeyPair::generate().map_err(|e| e.to_string())?;
        let mut params = ca_params();
        params.not_before = ymd(Utc::now() - Duration::days(1));
        params.not_after = ymd(Utc::now() + Duration::days(3650));
        let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
        write(&ca_key_path, &key.serialize_pem())?;
        write(&ca_path, &cert.pem())?;
        write(&dir.join("ca-name.txt"), CA_NAME)?;
        key
    } else {
        KeyPair::from_pem(&read(&ca_key_path)?).map_err(|e| format!("the CA key: {e}"))?
    };

    let info_path = dir.join("server.json");
    let info: Option<ServerInfo> = std::fs::read(&info_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let fresh = info.as_ref().is_some_and(|i| {
        i.names == names && i.not_after - Utc::now().timestamp() > RENEW_DAYS * 86400
    });
    if fresh && !new_ca && dir.join("server.pem").exists() && dir.join("server.key.pem").exists() {
        return Ok(dir);
    }
    // the same subject and key as the CA certificate, so what it signs chains to it
    let issuer = Issuer::new(ca_params(), &ca_key);
    let key = KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = CertificateParams::new(names.to_vec()).map_err(|e| format!("the names {names:?}: {e}"))?;
    params.distinguished_name.push(DnType::CommonName, names.iter().find(|n| n.parse::<IpAddr>().is_err() && *n != "localhost").cloned().unwrap_or_else(|| "localhost".into()));
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    let not_after = Utc::now() + Duration::days(SERVER_DAYS);
    params.not_before = ymd(Utc::now() - Duration::days(1));
    params.not_after = ymd(not_after);
    let cert = params.signed_by(&key, &issuer).map_err(|e| e.to_string())?;
    write(&dir.join("server.key.pem"), &key.serialize_pem())?;
    // the chain: server certificate, then the CA
    write(&dir.join("server.pem"), &format!("{}{}", cert.pem(), read(&ca_path)?))?;
    write(&info_path, &serde_json::to_string_pretty(&ServerInfo { not_after: not_after.timestamp(), names: names.to_vec() }).unwrap())?;
    Ok(dir)
}

/// The rustls server configuration from a certificate chain and key (PEM files).
pub fn server_config(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("reading {}: {e}", cert.display()))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| format!("reading {}: {e}", cert.display()))?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("reading {}: {e}", key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("the certificate: {e}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ca_signs_a_server_certificate_that_rustls_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let names = vec!["localhost".to_string(), "127.0.0.1".to_string(), "mypc".to_string()];
        let tls = ensure_self_signed(dir.path(), &names).unwrap();
        let ca = std::fs::read(tls.join("ca.pem")).unwrap();
        server_config(&tls.join("server.pem"), &tls.join("server.key.pem")).unwrap();
        // made once: the CA stays, the server certificate isn't made again while it's fresh
        let server = std::fs::read(tls.join("server.pem")).unwrap();
        ensure_self_signed(dir.path(), &names).unwrap();
        assert_eq!(std::fs::read(tls.join("server.pem")).unwrap(), server);
        // another address: a new server certificate, the same CA
        ensure_self_signed(dir.path(), &[names.clone(), vec!["10.0.0.9".into()]].concat()).unwrap();
        assert_ne!(std::fs::read(tls.join("server.pem")).unwrap(), server);
        assert_eq!(std::fs::read(tls.join("ca.pem")).unwrap(), ca);
        // a client trusting only the CA accepts the server's chain
        use rustls_pki_types::pem::PemObject;
        use rustls::client::danger::ServerCertVerifier;
        let mut roots = rustls::RootCertStore::empty();
        roots.add(rustls_pki_types::CertificateDer::from_pem_slice(&ca).unwrap()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build().unwrap();
        let chain: Vec<_> = rustls_pki_types::CertificateDer::pem_file_iter(tls.join("server.pem")).unwrap().map(|c| c.unwrap()).collect();
        let name = rustls_pki_types::ServerName::try_from("mypc").unwrap();
        verifier.verify_server_cert(&chain[0], &chain[1..], &name, &[], rustls_pki_types::UnixTime::now()).unwrap();
    }
}
