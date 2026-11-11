//! Signatures (Tools ▸ Signatures). A new message, reply or forward starts with its From
//! address's default; changing From swaps it; the message window has a Signature menu to pick
//! another or none. Kept as one setting (`signatures`) that travels.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::library::Library;

pub const SIGNATURES_KEY: &str = "signatures";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Signature {
    pub name: String,
    /// Rich text (HTML, as the editor writes it).
    pub html: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Signatures {
    pub list: Vec<Signature>,
    /// From address (lowercase) → the name of its default signature.
    pub defaults: BTreeMap<String, String>,
    /// For addresses with none of their own: this one ("" = none).
    pub fallback: String,
}

impl Signatures {
    /// The signature a message from `address` starts with, if any.
    pub fn for_address(&self, address: &str) -> Option<&Signature> {
        let name = self.defaults.get(&address.to_lowercase()).unwrap_or(&self.fallback);
        self.list.iter().find(|s| &s.name == name)
    }
}

impl Library {
    pub fn signatures(&self) -> Result<Signatures> {
        Ok(match self.setting(SIGNATURES_KEY)? {
            Some(json) => serde_json::from_str(&json).unwrap_or_default(),
            None => Signatures::default(),
        })
    }

    /// Keeps the signatures (names must be different, defaults must name one of them).
    pub fn set_signatures(&self, s: &Signatures) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for sig in &s.list {
            if sig.name.trim().is_empty() {
                return Err(Error::Other("every signature needs a name".into()));
            }
            if !seen.insert(sig.name.to_lowercase()) {
                return Err(Error::Other(format!("two signatures are called \"{}\"", sig.name)));
            }
        }
        let mut s = s.clone();
        let known = |n: &String| n.is_empty() || s.list.iter().any(|x| &x.name == n);
        s.defaults = s.defaults.iter().filter(|(_, n)| known(n)).map(|(a, n)| (a.to_lowercase(), n.clone())).collect();
        if !known(&s.fallback) {
            s.fallback.clear();
        }
        self.set_setting(SIGNATURES_KEY, &serde_json::to_string(&s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_address_starts_with_its_own_signature() {
        let s = Signatures {
            list: vec![Signature { name: "Work".into(), html: "<p>Pat, Acme</p>".into() }, Signature { name: "Short".into(), html: "A.".into() }],
            defaults: [("pat@example.com".to_string(), "Work".to_string()), ("noc@example.com".to_string(), String::new())].into(),
            fallback: "Short".into(),
        };
        assert_eq!(s.for_address("Pat@Example.com").unwrap().name, "Work");
        assert!(s.for_address("noc@example.com").is_none(), "none chosen for this one");
        assert_eq!(s.for_address("other@x.com").unwrap().name, "Short");
    }
}
