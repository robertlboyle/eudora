//! Special ▸ Forget Sign-ins (classic Eudora's "Forget Passwords"): every sign-in and password
//! Eudora keeps in this computer's credential store goes (Microsoft 365 sign-ins, IMAP / POP3
//! and SMTP passwords). Mail, settings and the sync key stay; downloading and sending wait
//! until the user signs in or types the passwords again. Only on this computer.

use serde::Serialize;

use crate::error::Result;
use crate::library::Library;

/// One sign-in kept on this computer.
#[derive(Clone, Debug, Serialize)]
pub struct SignIn {
    /// "Microsoft 365 sign-in for pat@example.com", "IMAP password for pat@example.org"…
    pub what: String,
    secret: String,
    graph_account: Option<String>,
}

/// An account waiting for its sign-in on this computer (`Library::missing_sign_ins`).
#[derive(Clone, Debug, serde::Serialize)]
pub struct MissingSignIn {
    pub address: String,
    /// "microsoft" or "google" (signed in through the browser), "password" (IMAP / POP3: typed
    /// in the account's window).
    pub kind: String,
    /// "Microsoft 365", "Gmail", "IMAP"…
    pub kind_name: String,
}

impl Library {
    /// The sign-ins and passwords this computer keeps for the library's accounts.
    pub fn sign_ins(&self) -> Result<Vec<SignIn>> {
        let mut out = Vec::new();
        for a in self.accounts_counted(false)? {
            let Some(address) = a.address.clone() else { continue };
            let lower = address.to_lowercase();
            match a.kind.as_str() {
                "graph" => {
                    let secret = format!("graph:{address}");
                    if crate::graph::load_secret(&secret).is_some() {
                        out.push(SignIn { what: format!("Microsoft 365 sign-in for {address}"), secret, graph_account: Some(address) });
                    }
                }
                "imap" | "pop3" => {
                    for (secret, what) in [(format!("imap:{lower}"), a.kind_name), (format!("smtp:{lower}"), "SMTP (sending)")] {
                        if crate::graph::load_secret(&secret).is_some() {
                            out.push(SignIn { what: format!("{what} password for {address}"), secret, graph_account: None });
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// The accounts this computer has no sign-in or password for yet (after a restore on a new
    /// computer: Microsoft and Google sign-ins never travel in a backup, and passwords only when
    /// it was made with them). Shared mailboxes ride on the Microsoft sign-in, and accounts made
    /// from files need none.
    pub fn missing_sign_ins(&self) -> Result<Vec<MissingSignIn>> {
        let have = |names: &[String]| names.iter().any(|n| crate::graph::load_secret(n).is_some());
        let mut out = Vec::new();
        for a in self.accounts_counted(false)? {
            let Some(address) = a.address.clone() else { continue };
            let lower = address.to_lowercase();
            let kind = match a.kind.as_str() {
                "graph" if !have(&[format!("graph:{address}"), format!("graph:{lower}")]) => "microsoft",
                "gmail" if !have(&[format!("google:{address}"), format!("google:{lower}")]) => "google",
                "imap" | "pop3" if !have(&[format!("imap:{lower}")]) => "password",
                _ => continue,
            };
            out.push(MissingSignIn { address, kind: kind.into(), kind_name: a.kind_name.to_string() });
        }
        Ok(out)
    }

    /// Forgets them all. Returns what was forgotten.
    pub fn forget_sign_ins(&self) -> Result<Vec<String>> {
        let mut done = Vec::new();
        for s in self.sign_ins()? {
            match &s.graph_account {
                Some(account) => crate::graph::GraphClient::sign_out(account)?,
                None => crate::graph::delete_secret(&s.secret),
            }
            done.push(s.what);
        }
        Ok(done)
    }
}
