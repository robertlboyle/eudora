//! Senders whose pictures from the web always show (Settings ▸ Mail). Kept in the setting
//! `pictures` ({keep, senders, addressBook}); an entry is an address or "@domain" (every address
//! there).

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::library::Library;

/// The file an export writes, in the folder chosen.
pub const EXPORT_NAME: &str = "Eudora picture senders.txt";

/// The addresses and @domains in a list someone wrote or another program exported: one or more
/// a line, separated by commas, semicolons, or spaces; "Name <address>" taken as its address;
/// lines starting with # (and anything that isn't an address) left out. Lowercase, each once.
pub fn parse_list(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        for word in line.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
            let w = word.trim_matches(|c: char| matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']')).to_lowercase();
            let w = w.strip_prefix("mailto:").unwrap_or(&w).to_string();
            let ok = match w.split_once('@') {
                Some(("", domain)) => domain.contains('.') && !domain.contains('@'),
                Some((local, domain)) => !local.is_empty() && domain.contains('.') && !domain.contains('@'),
                None => false,
            };
            if ok && w.len() <= 254 && !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// Whether a sender's entry is on `list`: the address itself, or "@" its domain.
pub fn listed(list: &[String], from: &str) -> bool {
    let from = from.to_lowercase();
    let domain = from.rsplit_once('@').map(|(_, d)| format!("@{d}"));
    list.iter().any(|s| *s == from || Some(s) == domain.as_ref())
}

/// The setting's parts this needs: the senders and whether the Address Book counts.
pub fn read_setting(value: Option<&str>) -> (Vec<String>, bool) {
    let v = value.and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok()).unwrap_or_default();
    let senders = v["senders"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_lowercase)).collect()).unwrap_or_default();
    (senders, v["addressBook"].as_bool().unwrap_or(false))
}

/// Whether `from` is in the Address Book (any account's contact with that address).
pub fn in_address_book(conn: &rusqlite::Connection, from: &str) -> Result<bool> {
    if from.is_empty() {
        return Ok(false);
    }
    // (a card is JSON: the address appears as a quoted string)
    Ok(conn.query_row("SELECT EXISTS (SELECT 1 FROM cards WHERE instr(lower(card), ?1) > 0)", rusqlite::params![format!("\"{}\"", from.to_lowercase())], |r| r.get(0))?)
}

/// Whether pictures from `from` show by themselves, from the library's setting.
pub fn show_for(conn: &rusqlite::Connection, from: &str) -> Result<bool> {
    if from.is_empty() {
        return Ok(false);
    }
    let value: Option<String> = rusqlite::OptionalExtension::optional(conn.query_row("SELECT value FROM settings_items WHERE key = 'pictures'", [], |r| r.get(0)))?;
    let (senders, book) = read_setting(value.as_deref());
    Ok(listed(&senders, from) || (book && in_address_book(conn, from)?))
}

impl Library {
    fn picture_setting(&self) -> Result<serde_json::Value> {
        Ok(self.setting("pictures")?.and_then(|j| serde_json::from_str(&j).ok()).filter(serde_json::Value::is_object).unwrap_or_else(|| serde_json::json!({ "keep": "fetch", "senders": [] })))
    }

    /// Adds the addresses and @domains in a list file to the senders whose pictures always show.
    /// Returns (how many were added, how many are on the list now).
    pub fn import_picture_senders(&self, file: &Path) -> Result<(usize, usize)> {
        let meta = std::fs::metadata(file).map_err(|e| Error::Other(format!("couldn't read {}: {e}", file.display())))?;
        if meta.len() > 16 * 1024 * 1024 {
            return Err(Error::Other(format!("{} is too big to be a list of senders", file.display())));
        }
        let bytes = std::fs::read(file).map_err(|e| Error::Other(format!("couldn't read {}: {e}", file.display())))?;
        let found = parse_list(&String::from_utf8_lossy(&bytes));
        let mut v = self.picture_setting()?;
        let mut senders: Vec<String> = v["senders"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_lowercase)).collect()).unwrap_or_default();
        let before = senders.len();
        for s in found {
            if !senders.contains(&s) {
                senders.push(s);
            }
        }
        let added = senders.len() - before;
        v["senders"] = serde_json::json!(senders);
        if added > 0 {
            self.set_setting("pictures", &v.to_string())?;
        }
        Ok((added, senders.len()))
    }

    /// Writes the senders whose pictures always show into `dir` (one a line, sorted). Returns the
    /// file written.
    pub fn export_picture_senders(&self, dir: &Path) -> Result<PathBuf> {
        if !dir.is_dir() {
            return Err(Error::Other(format!("{} isn't a folder", dir.display())));
        }
        let (mut senders, _) = read_setting(self.setting("pictures")?.as_deref());
        senders.sort();
        let file = dir.join(EXPORT_NAME);
        let text = format!(
            "# Senders whose pictures from the web always show in Eudora: one address (or @domain) a line.\r\n# Import it in Eudora: Settings > Mail > Senders whose pictures always show > Import.\r\n{}",
            senders.iter().map(|s| format!("{s}\r\n")).collect::<String>()
        );
        std::fs::write(&file, text).map_err(|e| Error::Other(format!("couldn't write {}: {e}", file.display())))?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_are_read_liberally_and_domains_count() {
        let list = parse_list("# mine\r\nAnn Smith <Ann@Example.com>, bob@x.org; @news.example.com\nnot an address\nmailto:c@d.io ann@example.com\n@nodot\n");
        assert_eq!(list, ["ann@example.com", "bob@x.org", "@news.example.com", "c@d.io"]);
        assert!(listed(&list, "Ann@example.com"));
        assert!(listed(&list, "deals@news.example.com"));
        assert!(!listed(&list, "x@example.com"));
        assert_eq!(read_setting(Some(r#"{"keep":"fetch","senders":["A@b.c"],"addressBook":true}"#)), (vec!["a@b.c".to_string()], true));
        assert_eq!(read_setting(None), (Vec::new(), false));
    }

    #[test]
    fn import_adds_once_and_export_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("lib"), crate::library::Access::Write, &crate::progress::Silent).unwrap();
        let file = dir.path().join("in.txt");
        std::fs::write(&file, "a@b.com\n@c.org\n").unwrap();
        assert_eq!(library.import_picture_senders(&file).unwrap(), (2, 2));
        assert_eq!(library.import_picture_senders(&file).unwrap(), (0, 2));
        let out = library.export_picture_senders(dir.path()).unwrap();
        assert_eq!(parse_list(&std::fs::read_to_string(out).unwrap()), ["@c.org", "a@b.com"]);
        // the window's other choices in the setting are kept
        library.set_setting("pictures", r#"{"keep":"store","senders":[],"addressBook":true}"#).unwrap();
        library.import_picture_senders(&file).unwrap();
        let v: serde_json::Value = serde_json::from_str(&library.setting("pictures").unwrap().unwrap()).unwrap();
        assert_eq!((v["keep"].as_str(), v["addressBook"].as_bool()), (Some("store"), Some(true)));
    }
}
