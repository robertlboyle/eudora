//! Signing in to the web interface: one password (kept as an Argon2 hash), passkeys (WebAuthn:
//! Face ID, a fingerprint, Windows Hello, a security key), and a session per device.
//!
//! Kept in `<library>/web/auth.json`. Sessions are random 32-byte tokens in a cookie
//! (HttpOnly, Secure, SameSite=Strict); only their SHA-256 is stored. Repeated wrong
//! passwords lock sign-in for a while (per address, and for everyone after many).
//!
//! Passkeys are checked here with `ring` (already part of the build): ES256 (P-256) and RS256
//! keys, "none" attestation (the key itself is trusted on first use, as passkeys are meant to
//! be); the challenge, origin, relying party and user-presence flag are checked every time.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type Result<T> = std::result::Result<T, String>;

/// How long a device stays signed in.
pub const SESSION_DAYS: i64 = 30;
/// Wrong passwords from one address before it waits…
const TRIES_PER_ADDRESS: u32 = 5;
/// …this long.
const LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Wrong passwords from anywhere within the lockout time before everyone waits.
const TRIES_OVERALL: u32 = 30;
/// A passkey challenge is good for this long.
const CHALLENGE_LIFE: Duration = Duration::from_secs(300);

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).map_err(|_| "bad base64".to_string())
}

fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("the system's random numbers");
    b
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Passkey {
    /// Credential id (base64url).
    pub id: String,
    /// -7 (ES256) or -257 (RS256).
    pub alg: i64,
    /// ES256: the uncompressed point (65 bytes); RS256: n (base64url).
    pub key: String,
    /// RS256: e (base64url).
    pub exponent: String,
    pub sign_count: u32,
    pub name: String,
    pub created: i64,
    pub last_used: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// SHA-256 of the token (base64url); the token itself is only in the device's cookie.
    pub hash: String,
    /// The device ("Safari on iPhone"), from its User-Agent.
    pub device: String,
    pub address: String,
    pub created: i64,
    pub last_seen: i64,
    pub expires: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    /// Argon2id hash (PHC string); empty = not set yet.
    password: String,
    /// The WebAuthn user handle (random, base64url).
    user_handle: String,
    passkeys: Vec<Passkey>,
    sessions: Vec<Session>,
}

#[derive(Default)]
struct Tries {
    per_address: HashMap<IpAddr, (u32, Instant)>,
    overall: Vec<Instant>,
}

pub struct Auth {
    path: PathBuf,
    stored: Mutex<Stored>,
    tries: Mutex<Tries>,
    /// Passkey challenges handed out: challenge → (when, for registering?).
    challenges: Mutex<HashMap<Vec<u8>, (Instant, bool)>>,
}

impl Auth {
    pub fn open(library: &Path) -> Result<Auth> {
        let dir = library.join("web");
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        let path = dir.join("auth.json");
        let mut stored: Stored = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("reading {}: {e}", path.display()))?,
            Err(_) => Stored::default(),
        };
        if stored.user_handle.is_empty() {
            stored.user_handle = b64(&random(16));
        }
        let auth = Auth { path, stored: Mutex::new(stored), tries: Mutex::default(), challenges: Mutex::default() };
        auth.save()?;
        Ok(auth)
    }

    fn save(&self) -> Result<()> {
        let stored = self.stored.lock().unwrap();
        let tmp = self.path.with_extension("json.part");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&*stored).unwrap()).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("writing {}: {e}", self.path.display()))
    }

    pub fn password_set(&self) -> bool {
        !self.stored.lock().unwrap().password.is_empty()
    }

    pub fn passkey_count(&self) -> usize {
        self.stored.lock().unwrap().passkeys.len()
    }

    /// Sets (or changes) the password. At least 10 characters.
    pub fn set_password(&self, password: &str) -> Result<()> {
        if password.chars().count() < 10 {
            return Err("use at least 10 characters".into());
        }
        let hash = argon2::Argon2::default().hash_password(password.as_bytes()).map_err(|e| e.to_string())?.to_string();
        self.stored.lock().unwrap().password = hash;
        self.save()
    }

    /// Forgets the password and signs every device out (`eudora-web --reset-password`).
    pub fn reset(&self) -> Result<()> {
        {
            let mut s = self.stored.lock().unwrap();
            s.password.clear();
            s.sessions.clear();
        }
        self.save()
    }

    /// Whether `address` may try a password now (None) or how long until it may.
    pub fn locked_out(&self, address: IpAddr) -> Option<Duration> {
        let mut tries = self.tries.lock().unwrap();
        let since = Instant::now() - LOCKOUT;
        tries.overall.retain(|t| *t > since);
        let mut wait = None;
        if tries.overall.len() as u32 >= TRIES_OVERALL {
            wait = Some(LOCKOUT - tries.overall[0].elapsed());
        }
        if let Some((n, first)) = tries.per_address.get(&address) {
            if first.elapsed() > LOCKOUT {
                tries.per_address.remove(&address);
            } else if *n >= TRIES_PER_ADDRESS {
                wait = Some(wait.unwrap_or_default().max(LOCKOUT - first.elapsed()));
            }
        }
        wait
    }

    fn failed(&self, address: IpAddr) {
        let mut tries = self.tries.lock().unwrap();
        tries.overall.push(Instant::now());
        let entry = tries.per_address.entry(address).or_insert((0, Instant::now()));
        entry.0 += 1;
    }

    /// Checks the password; a match starts a session (returns its token for the cookie).
    pub fn sign_in(&self, password: &str, address: IpAddr, device: &str) -> Result<String> {
        if let Some(wait) = self.locked_out(address) {
            return Err(format!("too many wrong passwords: try again in {} minutes", wait.as_secs().div_ceil(60)));
        }
        let hash = self.stored.lock().unwrap().password.clone();
        if hash.is_empty() {
            return Err("no password is set yet".into());
        }
        if PasswordVerifier::<str>::verify_password(&argon2::Argon2::default(), password.as_bytes(), &hash).is_err() {
            self.failed(address);
            return Err("that password isn't right".into());
        }
        self.tries.lock().unwrap().per_address.remove(&address);
        self.start_session(address, device)
    }

    fn start_session(&self, address: IpAddr, device: &str) -> Result<String> {
        let token = b64(&random(32));
        {
            let mut s = self.stored.lock().unwrap();
            let t = now();
            s.sessions.retain(|x| x.expires > t);
            s.sessions.push(Session {
                hash: b64(&sha256(token.as_bytes())),
                device: device.chars().take(120).collect(),
                address: address.to_string(),
                created: t,
                last_seen: t,
                expires: t + SESSION_DAYS * 86400,
            });
        }
        self.save()?;
        Ok(token)
    }

    /// The session for a cookie's token, if it's good (its last-seen time is updated now and then).
    pub fn session(&self, token: &str) -> Option<Session> {
        let hash = b64(&sha256(token.as_bytes()));
        let t = now();
        let mut s = self.stored.lock().unwrap();
        let session = s.sessions.iter_mut().find(|x| x.hash == hash && x.expires > t)?;
        let stale = t - session.last_seen > 3600;
        session.last_seen = t;
        let found = session.clone();
        drop(s);
        if stale {
            let _ = self.save();
        }
        Some(found)
    }

    pub fn sessions(&self) -> Vec<Session> {
        let t = now();
        self.stored.lock().unwrap().sessions.iter().filter(|x| x.expires > t).cloned().collect()
    }

    /// Signs a device out (by its session's hash).
    pub fn end_session(&self, hash: &str) -> Result<()> {
        self.stored.lock().unwrap().sessions.retain(|x| x.hash != hash);
        self.save()
    }

    pub fn end_session_token(&self, token: &str) -> Result<()> {
        self.end_session(&b64(&sha256(token.as_bytes())))
    }

    pub fn passkeys(&self) -> Vec<Passkey> {
        self.stored.lock().unwrap().passkeys.clone()
    }

    pub fn remove_passkey(&self, id: &str) -> Result<()> {
        self.stored.lock().unwrap().passkeys.retain(|p| p.id != id);
        self.save()
    }

    // ------------------------------------------------------------------ passkeys

    fn challenge(&self, register: bool) -> Vec<u8> {
        let c = random(32);
        let mut all = self.challenges.lock().unwrap();
        all.retain(|_, (t, _)| t.elapsed() < CHALLENGE_LIFE);
        all.insert(c.clone(), (Instant::now(), register));
        c
    }

    fn take_challenge(&self, challenge: &[u8], register: bool) -> Result<()> {
        match self.challenges.lock().unwrap().remove(challenge) {
            Some((t, r)) if r == register && t.elapsed() < CHALLENGE_LIFE => Ok(()),
            _ => Err("the passkey request ran out or was already used: try again".into()),
        }
    }

    /// What `navigator.credentials.create` needs to add a passkey (signed-in devices only).
    pub fn register_options(&self, rp_id: &str) -> serde_json::Value {
        let s = self.stored.lock().unwrap();
        let exclude: Vec<serde_json::Value> = s.passkeys.iter().map(|p| serde_json::json!({ "type": "public-key", "id": p.id })).collect();
        let user = s.user_handle.clone();
        drop(s);
        serde_json::json!({
            "challenge": b64(&self.challenge(true)),
            "rp": { "id": rp_id, "name": "Eudora" },
            "user": { "id": user, "name": "eudora", "displayName": "Eudora" },
            "pubKeyCredParams": [{ "type": "public-key", "alg": -7 }, { "type": "public-key", "alg": -257 }],
            "authenticatorSelection": { "residentKey": "preferred", "userVerification": "required" },
            "attestation": "none",
            "excludeCredentials": exclude,
            "timeout": 120000,
        })
    }

    /// What `navigator.credentials.get` needs to sign in with a passkey.
    pub fn login_options(&self, rp_id: &str) -> serde_json::Value {
        let allow: Vec<serde_json::Value> = self.stored.lock().unwrap().passkeys.iter().map(|p| serde_json::json!({ "type": "public-key", "id": p.id })).collect();
        serde_json::json!({
            "challenge": b64(&self.challenge(false)),
            "rpId": rp_id,
            "allowCredentials": allow,
            "userVerification": "required",
            "timeout": 120000,
        })
    }

    /// Adds a passkey from `navigator.credentials.create`'s answer.
    pub fn register(&self, rp_id: &str, origin: &str, answer: &RegisterAnswer) -> Result<()> {
        let client = check_client_data(&unb64(&answer.client_data_json)?, "webauthn.create", origin)?;
        self.take_challenge(&client, true)?;
        let attestation: ciborium::Value = ciborium::from_reader(unb64(&answer.attestation_object)?.as_slice()).map_err(|_| "unreadable attestation".to_string())?;
        let auth_data = cbor_get(&attestation, "authData").and_then(|v| v.as_bytes().cloned()).ok_or("no authenticator data")?;
        let parsed = parse_auth_data(&auth_data, rp_id)?;
        let (cred_id, key) = parsed.credential.ok_or("the passkey sent no key")?;
        let (alg, key, exponent) = cose_key(&key)?;
        let id = b64(&cred_id);
        {
            let mut s = self.stored.lock().unwrap();
            if s.passkeys.iter().any(|p| p.id == id) {
                return Err("that passkey is already added".into());
            }
            let name = if answer.name.trim().is_empty() { format!("Passkey {}", s.passkeys.len() + 1) } else { answer.name.trim().chars().take(60).collect() };
            s.passkeys.push(Passkey { id, alg, key: b64(&key), exponent: b64(&exponent), sign_count: parsed.sign_count, name, created: now(), last_used: 0 });
        }
        self.save()
    }

    /// Checks `navigator.credentials.get`'s answer; good = a new session (its token).
    pub fn passkey_sign_in(&self, rp_id: &str, origin: &str, answer: &LoginAnswer, address: IpAddr, device: &str) -> Result<String> {
        if let Some(wait) = self.locked_out(address) {
            return Err(format!("too many failed sign-ins: try again in {} minutes", wait.as_secs().div_ceil(60)));
        }
        let result = self.check_assertion(rp_id, origin, answer);
        if result.is_err() {
            self.failed(address);
        }
        result?;
        self.start_session(address, device)
    }

    fn check_assertion(&self, rp_id: &str, origin: &str, answer: &LoginAnswer) -> Result<()> {
        let client_json = unb64(&answer.client_data_json)?;
        let challenge = check_client_data(&client_json, "webauthn.get", origin)?;
        self.take_challenge(&challenge, false)?;
        let auth_data = unb64(&answer.authenticator_data)?;
        let parsed = parse_auth_data(&auth_data, rp_id)?;
        let key = self.passkeys().into_iter().find(|p| p.id == answer.id).ok_or("that passkey isn't known here")?;
        let mut signed = auth_data.clone();
        signed.extend(sha256(&client_json));
        let signature = unb64(&answer.signature)?;
        let good = match key.alg {
            -7 => ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, unb64(&key.key)?).verify(&signed, &signature).is_ok(),
            -257 => ring::signature::RsaPublicKeyComponents { n: unb64(&key.key)?, e: unb64(&key.exponent)? }
                .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, &signed, &signature)
                .is_ok(),
            _ => false,
        };
        if !good {
            return Err("the passkey's signature isn't right".into());
        }
        // a counter that didn't go up = a cloned key (0 = the authenticator doesn't count)
        if parsed.sign_count != 0 && parsed.sign_count <= key.sign_count {
            return Err("this passkey's counter went backward: it may have been copied".into());
        }
        {
            let mut s = self.stored.lock().unwrap();
            if let Some(p) = s.passkeys.iter_mut().find(|p| p.id == answer.id) {
                p.sign_count = parsed.sign_count;
                p.last_used = now();
            }
        }
        self.save()
    }
}

#[derive(Deserialize)]
pub struct RegisterAnswer {
    pub client_data_json: String,
    pub attestation_object: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Deserialize)]
pub struct LoginAnswer {
    pub id: String,
    pub client_data_json: String,
    pub authenticator_data: String,
    pub signature: String,
}

/// Checks the browser's client data (type, origin) and returns its challenge.
fn check_client_data(json: &[u8], kind: &str, origin: &str) -> Result<Vec<u8>> {
    let v: serde_json::Value = serde_json::from_slice(json).map_err(|_| "unreadable client data")?;
    if v["type"] != kind {
        return Err("not the expected passkey request".into());
    }
    if v["origin"] != origin {
        return Err(format!("the passkey answered for {}, not {origin}", v["origin"]));
    }
    unb64(v["challenge"].as_str().ok_or("no challenge")?)
}

struct AuthData {
    sign_count: u32,
    /// (credential id, COSE key) when registering.
    credential: Option<(Vec<u8>, ciborium::Value)>,
}

fn parse_auth_data(d: &[u8], rp_id: &str) -> Result<AuthData> {
    if d.len() < 37 {
        return Err("authenticator data too short".into());
    }
    if d[..32] != sha256(rp_id.as_bytes())[..] {
        return Err("the passkey belongs to another site".into());
    }
    let flags = d[32];
    if flags & 0x01 == 0 {
        return Err("the passkey wasn't confirmed (user presence)".into());
    }
    if flags & 0x04 == 0 {
        return Err("the passkey didn't verify you (Face ID, fingerprint or PIN)".into());
    }
    let sign_count = u32::from_be_bytes([d[33], d[34], d[35], d[36]]);
    let credential = if flags & 0x40 != 0 {
        // attested credential data: AAGUID (16), id length (2), id, COSE key
        let rest = d.get(37 + 16..).ok_or("no credential data")?;
        let len = u16::from_be_bytes([*rest.first().ok_or("no credential id")?, *rest.get(1).ok_or("no credential id")?]) as usize;
        let id = rest.get(2..2 + len).ok_or("credential id cut short")?.to_vec();
        let key: ciborium::Value = ciborium::from_reader(&rest[2 + len..]).map_err(|_| "unreadable passkey key")?;
        Some((id, key))
    } else {
        None
    };
    Ok(AuthData { sign_count, credential })
}

fn cbor_get<'a>(map: &'a ciborium::Value, key: &str) -> Option<&'a ciborium::Value> {
    map.as_map()?.iter().find(|(k, _)| k.as_text() == Some(key)).map(|(_, v)| v)
}

fn cbor_int<'a>(map: &'a ciborium::Value, key: i64) -> Option<&'a ciborium::Value> {
    map.as_map()?.iter().find(|(k, _)| k.as_integer().and_then(|i| i64::try_from(i).ok()) == Some(key)).map(|(_, v)| v)
}

/// A COSE public key → (alg, key bytes, RSA exponent).
fn cose_key(key: &ciborium::Value) -> Result<(i64, Vec<u8>, Vec<u8>)> {
    let int = |k: i64| cbor_int(key, k).and_then(|v| v.as_integer()).and_then(|i| i64::try_from(i).ok());
    let bytes = |k: i64| cbor_int(key, k).and_then(|v| v.as_bytes().cloned());
    match (int(1), int(3)) {
        // EC2, ES256, curve P-256
        (Some(2), Some(-7)) => {
            if int(-1) != Some(1) {
                return Err("only P-256 passkeys are supported".into());
            }
            let (x, y) = (bytes(-2).ok_or("no x")?, bytes(-3).ok_or("no y")?);
            if x.len() != 32 || y.len() != 32 {
                return Err("a malformed passkey key".into());
            }
            let mut point = vec![0x04];
            point.extend(x);
            point.extend(y);
            Ok((-7, point, Vec::new()))
        }
        // RSA, RS256
        (Some(3), Some(-257)) => Ok((-257, bytes(-1).ok_or("no n")?, bytes(-2).ok_or("no e")?)),
        _ => Err("this passkey's kind of key isn't supported (ES256 or RS256)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    #[test]
    fn passwords_sessions_and_lockout() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        assert!(!auth.password_set());
        assert!(auth.set_password("short").is_err());
        auth.set_password("a long enough password").unwrap();
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        let token = auth.sign_in("a long enough password", ip, "test").unwrap();
        assert!(auth.session(&token).is_some());
        assert!(auth.session("nope").is_none());
        // the stored file has the hash, never the password or the token
        let stored = std::fs::read_to_string(dir.path().join("web/auth.json")).unwrap();
        assert!(!stored.contains("a long enough password") && !stored.contains(&token) && stored.contains("$argon2"));
        // still there after opening again
        let again = Auth::open(dir.path()).unwrap();
        assert!(again.session(&token).is_some());
        // five wrong tries lock this address out, not another one
        for _ in 0..TRIES_PER_ADDRESS {
            assert!(again.sign_in("wrong", ip, "test").is_err());
        }
        assert!(again.sign_in("a long enough password", ip, "test").unwrap_err().contains("too many"));
        assert!(again.sign_in("a long enough password", "10.0.0.6".parse().unwrap(), "test").is_ok());
        again.end_session_token(&token).unwrap();
        assert!(again.session(&token).is_none());
        again.reset().unwrap();
        assert!(!again.password_set() && again.sessions().is_empty());
    }

    /// A software passkey (what a phone does), through registration and sign-in.
    #[test]
    fn a_passkey_is_added_and_signs_in() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        let (rp, origin) = ("mypc", "https://mypc");
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let point = key.public_key().as_ref().to_vec();
        let cred_id = b"credential-1".to_vec();

        // registration
        let options = auth.register_options(rp);
        let challenge = options["challenge"].as_str().unwrap();
        let client = serde_json::json!({ "type": "webauthn.create", "challenge": challenge, "origin": origin }).to_string();
        let cose = ciborium::Value::Map(vec![
            (1.into(), 2.into()),
            (3.into(), (-7).into()),
            ((-1).into(), 1.into()),
            ((-2).into(), ciborium::Value::Bytes(point[1..33].to_vec())),
            ((-3).into(), ciborium::Value::Bytes(point[33..].to_vec())),
        ]);
        let mut auth_data = sha256(rp.as_bytes());
        auth_data.push(0x01 | 0x04 | 0x40);
        auth_data.extend(0u32.to_be_bytes());
        auth_data.extend([0u8; 16]);
        auth_data.extend((cred_id.len() as u16).to_be_bytes());
        auth_data.extend(&cred_id);
        ciborium::into_writer(&cose, &mut auth_data).unwrap();
        let att = ciborium::Value::Map(vec![("fmt".into(), "none".into()), ("authData".into(), ciborium::Value::Bytes(auth_data))]);
        let mut att_bytes = Vec::new();
        ciborium::into_writer(&att, &mut att_bytes).unwrap();
        let answer = RegisterAnswer { client_data_json: b64(client.as_bytes()), attestation_object: b64(&att_bytes), name: "phone".into() };
        auth.register(rp, origin, &answer).unwrap();
        assert!(auth.register(rp, origin, &answer).is_err(), "a challenge works once");
        assert_eq!(auth.passkeys()[0].name, "phone");

        // sign-in
        let sign_in = |count: u32, origin_used: &str| {
            let options = auth.login_options(rp);
            let client = serde_json::json!({ "type": "webauthn.get", "challenge": options["challenge"], "origin": origin_used }).to_string();
            let mut data = sha256(rp.as_bytes());
            data.push(0x01 | 0x04);
            data.extend(count.to_be_bytes());
            let mut signed = data.clone();
            signed.extend(sha256(client.as_bytes()));
            let signature = key.sign(&rng, &signed).unwrap();
            let answer = LoginAnswer { id: b64(&cred_id), client_data_json: b64(client.as_bytes()), authenticator_data: b64(&data), signature: b64(signature.as_ref()) };
            auth.passkey_sign_in(rp, origin, &answer, "10.0.0.7".parse().unwrap(), "phone")
        };
        let token = sign_in(1, origin).unwrap();
        assert!(auth.session(&token).is_some());
        assert!(sign_in(1, origin).is_err(), "the counter must go up");
        assert!(sign_in(5, "https://evil.example").is_err(), "another site's origin");
        assert!(sign_in(6, origin).is_ok());
    }
}
