//! Finds the mail inside the paths given to an import: mbox files (any extension, found by
//! their first line), EML files (grouped by folder), and folders of both.

use std::path::{Path, PathBuf};

use crate::error::{IoContext, Result};
use crate::mbox::MboxFile;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// One mbox file (also Eudora .mbx, Thunderbird, Takeout).
    Mbox,
    /// EML files of one folder, one message per file.
    Eml,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub kind: SourceKind,
    /// Folder path below the import's destination ("Clients/Acme"); "" = the destination.
    pub folder: String,
    pub files: Vec<PathBuf>,
}

/// What `discover` found, plus the files it left out and why.
pub struct Discovery {
    pub sources: Vec<Source>,
    pub skipped: Vec<(PathBuf, String)>,
}

/// Files that sit next to mailboxes but never hold mail.
const NOT_MAIL: &[&str] = &["toc", "msf", "idx", "ini", "dat", "db", "pce", "lock", "json", "sqlite", "plist", "log"];

enum FileKind {
    Mbox,
    Eml,
    NotMail(String),
}

fn classify(path: &Path) -> Result<FileKind> {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    if let Some(why) = classic_backup(path) {
        return Ok(FileKind::NotMail(why));
    }
    if NOT_MAIL.contains(&ext.as_str()) {
        return Ok(FileKind::NotMail(format!(".{ext} files don't hold messages")));
    }
    let mbox = MboxFile::open(path)?;
    if mbox.looks_like_mbox() {
        return Ok(FileKind::Mbox);
    }
    if ext == "eml" || looks_like_message(mbox.data()) {
        return Ok(FileKind::Eml);
    }
    if mbox.data().is_empty() {
        return Ok(FileKind::NotMail("empty file".into()));
    }
    Ok(FileKind::NotMail("not a mailbox or message file".into()))
}

/// Classic Eudora's backups of In and Out ("In.mbx.001", "Out.toc.002"): before each Compact it
/// copies those mailboxes aside (fileutil.cpp `CascadeBackupFile`, setting InOutBackups, two by
/// default). They're old copies of the mailbox beside them, not more mail: importing them would
/// bring back messages deleted since as a folder of duplicates. Listed as left out, with why.
fn classic_backup(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let (stem, number) = name.rsplit_once('.')?;
    if number.len() != 3 || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if stem.ends_with(".mbx") || stem.ends_with(".toc") {
        let mailbox = &path.file_name()?.to_string_lossy()[..stem.len()];
        return Some(format!("classic Eudora's backup of {mailbox} from before a Compact (the mailbox itself holds the mail)"));
    }
    None
}

/// Starts with "Name: value" header lines (at least two of the usual ones).
fn looks_like_message(data: &[u8]) -> bool {
    let head = &data[..data.len().min(8192)];
    let known = [&b"from:"[..], b"to:", b"subject:", b"date:", b"message-id:", b"received:", b"return-path:", b"mime-version:"];
    let mut found = 0;
    for line in head.split(|b| *b == b'\n').take(60) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let lower: Vec<u8> = line.iter().take(16).map(u8::to_ascii_lowercase).collect();
        if known.iter().any(|k| lower.starts_with(k)) {
            found += 1;
        }
    }
    found >= 2
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Mailbox".into())
}

fn join(parts: &[String]) -> String {
    parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join("/")
}

pub fn discover(paths: &[PathBuf]) -> Result<Discovery> {
    let mut sources: Vec<Source> = Vec::new();
    let mut skipped = Vec::new();
    for path in paths {
        let meta = std::fs::metadata(path).context(|| format!("reading {}", path.display()))?;
        if meta.is_file() {
            match classify(path)? {
                FileKind::Mbox => sources.push(Source { kind: SourceKind::Mbox, folder: stem(path), files: vec![path.clone()] }),
                FileKind::Eml => push_eml(&mut sources, String::new(), path.clone()),
                FileKind::NotMail(why) => skipped.push((path.clone(), why)),
            }
            continue;
        }
        let mut entries: Vec<PathBuf> = walkdir::WalkDir::new(path)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .collect();
        entries.sort();
        for file in entries {
            let relative = file.strip_prefix(path).unwrap_or(&file);
            let dirs: Vec<String> = relative
                .parent()
                .map(|p| p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            match classify(&file)? {
                FileKind::Mbox => {
                    let mut parts = dirs.clone();
                    parts.push(stem(&file));
                    sources.push(Source { kind: SourceKind::Mbox, folder: join(&parts), files: vec![file] });
                }
                FileKind::Eml => push_eml(&mut sources, join(&dirs), file),
                FileKind::NotMail(why) => skipped.push((file, why)),
            }
        }
    }
    Ok(Discovery { sources, skipped })
}

fn push_eml(sources: &mut Vec<Source>, folder: String, file: PathBuf) {
    if let Some(source) = sources.iter_mut().rev().find(|s| s.kind == SourceKind::Eml && s.folder == folder) {
        source.files.push(file);
    } else {
        sources.push(Source { kind: SourceKind::Eml, folder, files: vec![file] });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_mailboxes_and_eml_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Clients/Acme")).unwrap();
        std::fs::write(root.join("In.mbx"), b"From a@b Sat Jan  3 01:05:34 1996\nSubject: x\n\ny\n").unwrap();
        std::fs::write(root.join("In.toc"), b"\x00\x01binary").unwrap();
        std::fs::write(root.join("Clients/Acme/1.eml"), b"From: a@b.com\nSubject: one\n\nhi\n").unwrap();
        std::fs::write(root.join("Clients/Acme/2.eml"), b"From: a@b.com\nSubject: two\n\nhi\n").unwrap();
        std::fs::write(root.join("Clients/notes.txt"), b"just some notes").unwrap();
        let found = discover(&[root.to_path_buf()]).unwrap();
        let summary: Vec<(SourceKind, String, usize)> = found.sources.iter().map(|s| (s.kind.clone(), s.folder.clone(), s.files.len())).collect();
        assert_eq!(summary, vec![(SourceKind::Eml, "Clients/Acme".into(), 2), (SourceKind::Mbox, "In".into(), 1)]);
        assert_eq!(found.skipped.len(), 2);
    }

    #[test]
    fn classic_backups_of_in_and_out_are_left_out_and_said() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mail = b"From ???@??? Sat Jan 03 01:05:34 1996\r\nSubject: x\r\n\r\ny\r\n";
        std::fs::write(root.join("In.mbx"), mail).unwrap();
        std::fs::write(root.join("In.mbx.001"), mail).unwrap();
        std::fs::write(root.join("In.toc.001"), b"\x31\x00").unwrap();
        std::fs::write(root.join("Out.mbx.002"), mail).unwrap();
        let found = discover(&[root.to_path_buf()]).unwrap();
        assert_eq!(found.sources.iter().map(|s| s.folder.as_str()).collect::<Vec<_>>(), vec!["In"]);
        let why: Vec<&str> = found.skipped.iter().map(|(_, w)| w.as_str()).collect();
        assert_eq!(why.iter().filter(|w| w.contains("backup of In.mbx")).count(), 1);
        assert!(why.iter().any(|w| w.contains("backup of Out.mbx")));
        assert!(why.iter().any(|w| w.contains("backup of In.toc")));
    }
}
