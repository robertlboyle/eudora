//! Eudora's credential store on Android (sign-ins, mail server passwords, keys the user typed).
//! The `keyring` crate has no Android store, so, as for iOS (`graph::keyring_entry`), Eudora
//! hands `keyring-core` one of its own: each secret is a file in the app's private storage
//! (`<app data>/secrets/`), which Android lets no other app read, and which the app's manifest
//! keeps out of Android's backups (`allowBackup="false"`), so a sign-in never leaves the phone.
//!
//! Each file is also **sealed with a key kept in the Android Keystore** (made there, never
//! readable by any app): the app hands this store a sealer (`set_sealer`; `SecretBox.kt` through
//! JNI, the core has none of its own), so a file copied off the phone is useless. A file written
//! before (no `SEALED` mark) is still read, and sealed when it's next written; one that can't be
//! opened any more (the app reinstalled: a new Keystore key) reads as missing, so you sign in again.

use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use keyring_core::api::{CredentialApi, CredentialStoreApi};
use keyring_core::{Entry, Error, Result};

/// Seals or opens a secret with the Keystore's key.
pub type Sealer = Box<dyn Fn(&[u8], bool) -> std::result::Result<Vec<u8>, String> + Send + Sync>;
static SEALER: OnceLock<Sealer> = OnceLock::new();

/// The app's sealer (`true` = seal, `false` = open), set once at the start.
pub fn set_sealer(sealer: Sealer) {
    let _ = SEALER.set(sealer);
}

/// The mark at the start of a sealed file.
const SEALED: &[u8] = b"EUDORA-SEALED-1\n";

/// The store: one folder of files, one per (service, user).
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    /// (one writer at a time within the app)
    lock: Arc<Mutex<()>>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Arc<Store> {
        Arc::new(Store { dir, lock: Arc::new(Mutex::new(())) })
    }
}

#[derive(Debug)]
struct Secret {
    file: PathBuf,
    service: String,
    user: String,
    lock: Arc<Mutex<()>>,
}

fn failure(e: std::io::Error) -> Error {
    Error::PlatformFailure(Box::new(e))
}

impl CredentialApi for Secret {
    fn set_secret(&self, secret: &[u8]) -> Result<()> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(failure)?;
        }
        // sealed with the Keystore's key when the app gave one
        let bytes = match SEALER.get() {
            Some(seal) => [SEALED, &seal(secret, true).map_err(|e| Error::PlatformFailure(e.into()))?].concat(),
            None => secret.to_vec(),
        };
        // written whole, then put in place: never half a secret
        let tmp = self.file.with_extension("new");
        std::fs::write(&tmp, bytes).map_err(failure)?;
        std::fs::rename(&tmp, &self.file).map_err(failure)
    }

    fn get_secret(&self) -> Result<Vec<u8>> {
        match std::fs::read(&self.file) {
            Ok(bytes) => match bytes.strip_prefix(SEALED) {
                // (one that can't be opened, a new Keystore key after a reinstall: as if missing)
                Some(sealed) => match SEALER.get() {
                    Some(open) => open(sealed, false).map_err(|_| Error::NoEntry),
                    None => Err(Error::NoEntry),
                },
                None => Ok(bytes),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NoEntry),
            Err(e) => Err(failure(e)),
        }
    }

    fn delete_credential(&self) -> Result<()> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        match std::fs::remove_file(&self.file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NoEntry),
            Err(e) => Err(failure(e)),
        }
    }

    fn get_credential(&self) -> Result<Option<Arc<keyring_core::api::Credential>>> {
        if self.file.exists() { Ok(None) } else { Err(Error::NoEntry) }
    }

    fn get_specifiers(&self) -> Option<(String, String)> {
        Some((self.service.clone(), self.user.clone()))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl CredentialStoreApi for Store {
    fn vendor(&self) -> String {
        "Eudora's Android store (app-private files)".into()
    }

    fn id(&self) -> String {
        self.dir.display().to_string()
    }

    fn build(&self, service: &str, user: &str, _modifiers: Option<&HashMap<&str, &str>>) -> Result<Entry> {
        // the file's name: a hash of the two (any characters may be in them)
        let name = blake3::hash(format!("{service}\0{user}").as_bytes()).to_hex();
        let secret = Secret { file: self.dir.join(format!("{}.secret", &name[..32])), service: service.into(), user: user.into(), lock: self.lock.clone() };
        Ok(Entry::new_with_credential(Arc::new(secret)))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_kept_sealed_read_and_deleted() {
        // (a stand-in for SecretBox: bytes reversed, a mark that must come back)
        set_sealer(Box::new(|bytes, seal| {
            if seal {
                Ok([b"box:".as_slice(), &bytes.iter().rev().copied().collect::<Vec<u8>>()].concat())
            } else {
                bytes.strip_prefix(b"box:").map(|b| b.iter().rev().copied().collect()).ok_or_else(|| "not ours".to_string())
            }
        }));
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("secrets"));
        let entry = store.build("Eudora", "graph:ann@example.com#0", None).unwrap();
        assert!(matches!(entry.get_secret(), Err(Error::NoEntry)));
        entry.set_secret(b"refresh token").unwrap();
        let file = std::fs::read_dir(dir.path().join("secrets")).unwrap().next().unwrap().unwrap().path();
        let raw = std::fs::read(&file).unwrap();
        assert!(raw.starts_with(SEALED) && !raw.windows(13).any(|w| w == b"refresh token"), "the file holds only the sealed secret");
        // a file from before sealing is still read
        std::fs::write(&file, b"old plain secret").unwrap();
        assert_eq!(entry.get_secret().unwrap(), b"old plain secret");
        // one that can't be opened (a new Keystore key) reads as missing
        std::fs::write(&file, [SEALED, b"garbage"].concat()).unwrap();
        assert!(matches!(entry.get_secret(), Err(Error::NoEntry)));
        entry.set_secret(b"refresh token").unwrap();
        assert_eq!(store.build("Eudora", "graph:ann@example.com#0", None).unwrap().get_secret().unwrap(), b"refresh token");
        assert!(matches!(store.build("Eudora", "graph:bob@example.com#0", None).unwrap().get_secret(), Err(Error::NoEntry)));
        entry.delete_credential().unwrap();
        assert!(matches!(entry.delete_credential(), Err(Error::NoEntry)));
    }
}
