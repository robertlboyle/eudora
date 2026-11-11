//! The sync envelope: what Eudora stores in a mailbox for its other computers (an account's
//! filing log, the settings), sealed with the user's sync key so the server, and anyone else
//! who can open the mailbox (colleagues in a shared one), sees only noise. Opening checks it's
//! whole and was sealed with this key for this purpose; anything else is refused.
//!
//! Sealing: a fresh 32-byte salt per envelope; HKDF-SHA256(sync key, salt, purpose) gives that
//! envelope's own key, used once with ChaCha20-Poly1305 (nonce 0). A key is never used twice,
//! which is what XChaCha20's long nonce (the first plan) was for, with `ring`, already in the
//! build. The purpose ("filing:me@x.com", "settings") is bound in as associated data, so an
//! envelope can't be passed off as another.
//!
//! The sync key: 32 random bytes, made once, kept in Windows Credential Manager / the macOS
//! Keychain (never in the library or the settings), shown to the user once as a recovery
//! phrase to type on their other computers. Its id (first 8 bytes of SHA-256) names the
//! envelopes, so two people syncing one shared mailbox each keep their own.

use ring::aead::{self, Aad, BoundKey, CHACHA20_POLY1305, Nonce, NonceSequence, UnboundKey};
use ring::hkdf;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

const MAGIC: &[u8; 8] = b"EUDSYNC1";
const SALT: usize = 32;
const KEY_ID: usize = 8;
/// Where the key is kept in the OS secret store.
const SECRET_NAME: &str = "sync-key";
const DEVICE_NAME: &str = "sync-device";
/// The recovery phrase's alphabet (Crockford's base 32: no I, L, O, U).
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Clone, PartialEq)]
pub struct SyncKey([u8; 32]);

impl std::fmt::Debug for SyncKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SyncKey({})", self.id()) // never the key itself
    }
}

impl SyncKey {
    /// A new random key.
    pub fn generate() -> Result<SyncKey> {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
        Ok(SyncKey(k))
    }

    /// The key's name (16 hex digits): names its envelopes, never reveals it.
    pub fn id(&self) -> String {
        hex(&Sha256::digest(self.0)[..KEY_ID])
    }

    /// The recovery phrase: the key and a 2-byte check in base 32, in groups of 5
    /// ("7K2QH-…", 55 characters), to type on another computer.
    pub fn phrase(&self) -> String {
        let mut bytes = self.0.to_vec();
        bytes.extend_from_slice(&Sha256::digest(self.0)[..2]);
        let chars: Vec<u8> = base32(&bytes);
        chars.chunks(5).map(|c| String::from_utf8_lossy(c).to_string()).collect::<Vec<_>>().join("-")
    }

    /// The key from its recovery phrase (spaces, dashes and case ignored; O read as 0, I and
    /// L as 1). A mistyped phrase is refused by its check.
    pub fn from_phrase(phrase: &str) -> Result<SyncKey> {
        let mut values = Vec::new();
        for c in phrase.chars().filter(|c| !c.is_whitespace() && *c != '-') {
            let c = match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            let v = ALPHABET.iter().position(|a| *a as char == c).ok_or_else(|| Error::Other(format!("“{c}” isn't in a recovery phrase")))?;
            values.push(v as u8);
        }
        let bytes = unbase32(&values);
        if bytes.len() < 34 {
            return Err(Error::Other("the recovery phrase is too short".into()));
        }
        let key: [u8; 32] = bytes[..32].try_into().unwrap();
        if Sha256::digest(key)[..2] != bytes[32..34] {
            return Err(Error::Other("the recovery phrase has a typing mistake: check it and type it again".into()));
        }
        Ok(SyncKey(key))
    }

    /// The key kept on this computer, if any.
    pub fn load() -> Option<SyncKey> {
        crate::graph::load_secret(SECRET_NAME).and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()).map(SyncKey)
    }

    /// Keeps the key on this computer (OS secret store).
    pub fn store(&self) -> Result<()> {
        crate::graph::store_secret(SECRET_NAME, &self.0)
    }

    /// Forgets the key on this computer (Change sync key, or a computer leaving).
    pub fn forget() {
        crate::graph::delete_secret(SECRET_NAME);
    }

    fn envelope_key(&self, salt: &[u8], purpose: &str) -> Result<UnboundKey> {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&self.0);
        let info = [purpose.as_bytes()];
        let okm = prk.expand(&info, &CHACHA20_POLY1305).map_err(|_| Error::Other("key derivation failed".into()))?;
        Ok(UnboundKey::from(okm))
    }

    /// Seals `data` for `purpose` ("filing:me@x.com", "settings").
    pub fn seal(&self, purpose: &str, data: &[u8]) -> Result<Vec<u8>> {
        let mut salt = [0u8; SALT];
        getrandom::fill(&mut salt).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
        let mut key = aead::SealingKey::new(self.envelope_key(&salt, purpose)?, Once(true));
        let mut out = Vec::with_capacity(MAGIC.len() + KEY_ID + SALT + data.len() + 16);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&Sha256::digest(self.0)[..KEY_ID]);
        out.extend_from_slice(&salt);
        let mut body = data.to_vec();
        key.seal_in_place_append_tag(Aad::from(aad(&out, purpose)), &mut body).map_err(|_| Error::Other("sealing failed".into()))?;
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Opens an envelope sealed with this key for `purpose`. Refused: not an envelope, another
    /// key's, another purpose's, or changed in any way.
    pub fn open(&self, purpose: &str, envelope: &[u8]) -> Result<Vec<u8>> {
        let head = MAGIC.len() + KEY_ID + SALT;
        if envelope.len() < head + 16 || &envelope[..MAGIC.len()] != MAGIC {
            return Err(Error::Other("not an Eudora sync envelope".into()));
        }
        if envelope[MAGIC.len()..MAGIC.len() + KEY_ID] != Sha256::digest(self.0)[..KEY_ID] {
            return Err(Error::Other("sealed with another sync key".into()));
        }
        let salt = &envelope[MAGIC.len() + KEY_ID..head];
        let mut key = aead::OpeningKey::new(self.envelope_key(salt, purpose)?, Once(true));
        let mut body = envelope[head..].to_vec();
        let plain = key
            .open_in_place(Aad::from(aad(&envelope[..head], purpose)), &mut body)
            .map_err(|_| Error::Other("the sync envelope was changed or isn't for this".into()))?;
        Ok(plain.to_vec())
    }
}

/// The key id of an envelope (whose key sealed it), without opening it.
pub fn envelope_key_id(envelope: &[u8]) -> Option<String> {
    (envelope.len() >= MAGIC.len() + KEY_ID && &envelope[..MAGIC.len()] == MAGIC).then(|| hex(&envelope[MAGIC.len()..MAGIC.len() + KEY_ID]))
}

/// This computer's name in sync logs: random, made once, kept in the OS secret store (so a
/// library copied to another computer doesn't bring it along).
pub fn device_id() -> Result<String> {
    if let Some(id) = crate::graph::load_secret(DEVICE_NAME).and_then(|b| String::from_utf8(b).ok()) {
        return Ok(id);
    }
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    let id = hex(&b);
    crate::graph::store_secret(DEVICE_NAME, id.as_bytes())?;
    Ok(id)
}

/// The associated data: the envelope's head and its purpose.
fn aad(head: &[u8], purpose: &str) -> Vec<u8> {
    let mut a = head.to_vec();
    a.extend_from_slice(purpose.as_bytes());
    a
}

/// One nonce, once: each envelope has a key of its own.
struct Once(bool);

impl NonceSequence for Once {
    fn advance(&mut self) -> std::result::Result<Nonce, ring::error::Unspecified> {
        if std::mem::replace(&mut self.0, false) { Ok(Nonce::assume_unique_for_key([0u8; 12])) } else { Err(ring::error::Unspecified) }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn base32(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in bytes {
        acc = (acc << 8) | u32::from(*b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize]);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize]);
    }
    out
}

fn unbase32(values: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for v in values {
        acc = (acc << 5) | u32::from(*v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_data_opens_only_with_its_key_and_purpose() {
        let key = SyncKey::generate().unwrap();
        let other = SyncKey::generate().unwrap();
        let env = key.seal("filing:me@x.com", b"the log").unwrap();
        assert_eq!(key.open("filing:me@x.com", &env).unwrap(), b"the log");
        assert_eq!(envelope_key_id(&env), Some(key.id()));
        assert!(other.open("filing:me@x.com", &env).is_err(), "another key");
        assert!(key.open("filing:you@x.com", &env).is_err(), "another purpose");
        let mut changed = env.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(key.open("filing:me@x.com", &changed).is_err(), "changed");
        assert!(key.open("filing:me@x.com", b"EUDSYNC1 junk").is_err());
        // a fresh salt each time: the same data never looks the same
        assert_ne!(key.seal("settings", b"x").unwrap(), key.seal("settings", b"x").unwrap());
    }

    #[test]
    fn the_recovery_phrase_brings_the_key_back_and_catches_typos() {
        let key = SyncKey::generate().unwrap();
        let phrase = key.phrase();
        assert_eq!(phrase.len(), 55 + 10, "{phrase}");
        assert_eq!(SyncKey::from_phrase(&phrase).unwrap(), key);
        // typed in lower case, with spaces, O for 0
        let typed = phrase.to_lowercase().replace('-', " ").replace('0', "o");
        assert_eq!(SyncKey::from_phrase(&typed).unwrap(), key);
        // one character wrong
        let mut wrong: Vec<char> = phrase.chars().collect();
        wrong[3] = if wrong[3] == 'A' { 'B' } else { 'A' };
        assert!(SyncKey::from_phrase(&wrong.into_iter().collect::<String>()).is_err());
        assert!(!format!("{key:?}").contains(&phrase), "never printed");
    }
}
