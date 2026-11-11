//! Opening and saving attachments.
//!
//! Opening writes the file into the library's `Attachments/<year>` folder (decided 2026-10-05,
//! like classic Eudora's Attachments folder; the year the message came) and hands it to the
//! system's program for its kind. Identical content already there is used again, never written
//! twice. Every file Eudora writes is marked as downloaded from the Internet (Windows' Mark of
//! the Web, the Mac's quarantine), so Windows, Office and the Mac treat it with the usual care.
//! Files that can run programs aren't opened from Eudora (they can be saved), and a message the
//! fraud checks flag never opens its attachments.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::library::Library;

/// Kinds of files that run programs or scripts when opened (Windows' and the Mac's), never
/// opened from a message: saving them is the user's choice.
const RUNNABLE: &[&str] = &[
    "exe", "com", "scr", "pif", "bat", "cmd", "msi", "msp", "msix", "appx", "appxbundle", "ps1", "psm1", "psd1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "hta",
    "cpl", "jar", "lnk", "reg", "inf", "dll", "sys", "chm", "url", "scf", "application", "gadget", "msc", "iso", "img", "vhd", "vhdx", "xll", "app", "command",
    "pkg", "dmg", "sh", "workflow", "terminal",
];

/// Whether opening a file of this name could run a program.
pub fn runnable(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.trim().to_ascii_lowercase()).unwrap_or_default();
    RUNNABLE.contains(&ext.as_str())
}

/// A name safe as a file name on every system: no folders in it, no characters Windows refuses,
/// no reserved device names, no trailing dots or spaces; "attachment" when nothing is left.
pub fn safe_file_name(name: &str) -> String {
    let mut out: String = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    out = out.trim().trim_end_matches(['.', ' ']).to_string();
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str()) || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        out = format!("_{out}");
    }
    if out.is_empty() || out.chars().all(|c| c == '.') {
        out = "attachment".into();
    }
    // (long names: Windows' 255 per name; the extension kept)
    if out.chars().count() > 180 {
        let (stem, ext) = out.rsplit_once('.').map(|(s, e)| (s.to_string(), format!(".{e}"))).unwrap_or((out.clone(), String::new()));
        out = format!("{}{ext}", stem.chars().take(180usize.saturating_sub(ext.chars().count())).collect::<String>());
    }
    out
}

/// Where to write `name` with these bytes in `dir`: the same file when one there already holds
/// them (true), else the name, or "name (2).ext", "name (3).ext"… (false).
pub fn place_for(dir: &Path, name: &str, bytes: &[u8]) -> (PathBuf, bool) {
    let (stem, ext) = name.rsplit_once('.').filter(|(s, _)| !s.is_empty()).map(|(s, e)| (s.to_string(), format!(".{e}"))).unwrap_or((name.to_string(), String::new()));
    for n in 1.. {
        let candidate = dir.join(if n == 1 { name.to_string() } else { format!("{stem} ({n}){ext}") });
        match std::fs::metadata(&candidate) {
            Err(_) => return (candidate, false),
            Ok(meta) if meta.len() == bytes.len() as u64 && std::fs::read(&candidate).is_ok_and(|b| b == bytes) => return (candidate, true),
            Ok(_) => {}
        }
    }
    unreachable!()
}

/// Marks a file as downloaded from the Internet: Windows' Mark of the Web (the Internet zone),
/// the Mac's quarantine flag; nothing elsewhere. (Per system by nature: no library does this.)
pub fn mark_downloaded(path: &Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let _ = std::fs::write(PathBuf::from(stream), b"[ZoneTransfer]\r\nZoneId=3\r\n");
    }
    #[cfg(target_os = "macos")]
    {
        let flag = format!("0081;{:x};Eudora;", crate::library::now());
        let _ = std::process::Command::new("xattr").arg("-w").arg("com.apple.quarantine").arg(flag).arg(path).status();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = path;
}

/// Writes one file (or finds it there already), marked as downloaded.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(|e| Error::Other(format!("{}: {e}", dir.display())))?;
    let (path, there) = place_for(dir, &safe_file_name(name), bytes);
    if !there {
        std::fs::write(&path, bytes).map_err(|e| Error::Other(format!("{}: {e}", path.display())))?;
    }
    mark_downloaded(&path);
    Ok(path)
}

impl Library {
    /// An attachment's name and bytes (the name as the message gives it).
    fn attachment_named(&self, id: i64, part: &str) -> Result<(String, Vec<u8>)> {
        let view = self.message_view(id)?;
        let name = view.attachments.iter().find(|a| a.part == part).map(|a| a.name.clone()).unwrap_or_else(|| "attachment".into());
        let (_, bytes) = self.attachment_bytes(id, part)?;
        Ok((name, bytes))
    }

    /// Saves one attachment into a folder the user chose; returns where it went.
    pub fn save_attachment(&self, id: i64, part: &str, dir: &Path) -> Result<PathBuf> {
        let (name, bytes) = self.attachment_named(id, part)?;
        write_file(dir, &name, &bytes)
    }

    /// Saves every attachment of a message into a folder (those Eudora has; a classic Eudora file
    /// never found is left out and said).
    pub fn save_all_attachments(&self, id: i64, dir: &Path) -> Result<(Vec<PathBuf>, Vec<String>)> {
        let view = self.message_view(id)?;
        let (mut saved, mut missing) = (Vec::new(), Vec::new());
        for a in &view.attachments {
            if a.missing.is_some() {
                missing.push(a.name.clone());
                continue;
            }
            let (_, bytes) = self.attachment_bytes(id, &a.part)?;
            saved.push(write_file(dir, &a.name, &bytes)?);
        }
        Ok((saved, missing))
    }

    /// Writes an attachment into `Attachments/<year>` to be opened with the system's program;
    /// refused for programs and scripts, and for any attachment of a message flagged as a fraud.
    pub fn attachment_to_open(&self, id: i64, part: &str) -> Result<PathBuf> {
        let (name, bytes) = self.attachment_named(id, part)?;
        if self.fraud_warning(&self.raw_message(id)?)?.is_some() {
            return Err(Error::Other("this message looks like a fraud: its attachments aren't opened. Mark it Spam, or save one only if you're sure it's safe.".into()));
        }
        if runnable(&name) {
            return Err(Error::Other(format!("“{name}” can run a program on your computer, so Eudora doesn't open it. Save it, and open it yourself only if you trust who sent it.")));
        }
        let date: i64 = self.conn.query_row("SELECT date FROM messages WHERE id = ?1", [id], |r| r.get(0))?;
        let year = chrono::DateTime::from_timestamp(date, 0).map(|d| d.format("%Y").to_string()).unwrap_or_else(|| "Undated".into());
        write_file(&self.root.join("Attachments").join(year), &name, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_file_name("..\\..\\Windows\\evil.txt"), "evil.txt");
        assert_eq!(safe_file_name("a/b/c:d?.pdf"), "c_d_.pdf");
        assert_eq!(safe_file_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_file_name("report.pdf. . "), "report.pdf");
        assert_eq!(safe_file_name(""), "attachment");
        assert_eq!(safe_file_name("..."), "attachment");
        assert!(safe_file_name(&format!("{}.pdf", "x".repeat(400))).ends_with(".pdf"));
    }

    #[test]
    fn programs_are_recognized() {
        assert!(runnable("invoice.pdf.exe") && runnable("x.JS") && runnable("setup.msi") && runnable("link.lnk"));
        assert!(!runnable("invoice.pdf") && !runnable("photo.jpg") && !runnable("README"));
    }

    #[test]
    fn identical_files_are_used_again_and_others_numbered() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = write_file(dir.path(), "a.txt", b"one").unwrap();
        let p2 = write_file(dir.path(), "a.txt", b"one").unwrap();
        let p3 = write_file(dir.path(), "a.txt", b"two").unwrap();
        assert_eq!(p1, p2);
        assert_eq!(p3.file_name().unwrap().to_str().unwrap(), "a (2).txt");
        assert_eq!(std::fs::read(&p3).unwrap(), b"two");
        #[cfg(windows)]
        {
            let mut zone = p1.as_os_str().to_owned();
            zone.push(":Zone.Identifier");
            assert!(std::fs::read_to_string(PathBuf::from(zone)).unwrap().contains("ZoneId=3"), "marked as from the Internet");
        }
    }
}
