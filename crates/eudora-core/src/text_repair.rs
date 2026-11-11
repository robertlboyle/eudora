//! Text in the library that isn't valid UTF-8, repaired.
//!
//! Found 2026-10-25: one stored text value with a broken byte made every check of new mail stop
//! ("invalid utf-8 sequence of 1 bytes from index 63 at index 1"), for hours: a server id whose
//! "B" had become the byte 0xCA (two bits flipped: the disk or memory, not Eudora's code).
//! Every text column of every table is looked at here, every value byte by byte, the way Rust reads
//! text (SQLite's own text functions step over such a byte: that id passed GLOB and length()
//! alike, so they can't find one), and a broken one is rewritten with only its bad bytes replaced
//! by "�". Run by maintenance (a few minutes on a big library) and at once when a check stops on
//! such a value.

use crate::error::Result;
use rusqlite::{Connection, params};

/// What a repair found: (table, column, how many values repaired).
pub type Repaired = Vec<(String, String, u64)>;

/// Repairs every text value that isn't valid UTF-8, in every table. Returns what it repaired.
pub fn repair(conn: &Connection) -> Result<Repaired> {
    let columns: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT m.name, p.name FROM sqlite_master m JOIN pragma_table_info(m.name) p
              WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' AND (p.type LIKE '%TEXT%' OR p.type = '')",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let mut repaired = Repaired::new();
    for (table, column) in columns {
        let q = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
        let broken: Vec<Vec<u8>> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT CAST({c} AS BLOB) FROM {t} WHERE typeof({c}) = 'text'",
                c = q(&column),
                t = q(&table)
            ))?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            let mut broken = Vec::new();
            for row in rows {
                let bytes = row?;
                if std::str::from_utf8(&bytes).is_err() {
                    broken.push(bytes);
                }
            }
            broken
        };
        if broken.is_empty() {
            continue;
        }
        let mut fixed = 0u64;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            // (matched by its bytes: tables without a rowid too; a repaired value that would clash
            // with one already there is left, never lost)
            let mut update = conn.prepare(&format!("UPDATE OR IGNORE {t} SET {c} = ?1 WHERE CAST({c} AS BLOB) = ?2", c = q(&column), t = q(&table)))?;
            for bytes in &broken {
                fixed += update.execute(params![String::from_utf8_lossy(bytes).into_owned(), bytes])? as u64;
            }
            Ok(())
        })();
        conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result?;
        repaired.push((table, column, fixed));
    }
    Ok(repaired)
}

/// Text from outside (a server, a file, a header) made safe to store: valid UTF-8, no NUL
/// characters (they cut text short in other programs), at most `max` bytes, cut on a character
/// boundary. "Liberal in what you accept, conservative in what you store."
pub fn clean(text: &str, max: usize) -> String {
    let mut out: String = text.chars().filter(|c| *c != '\0').collect();
    if out.len() > max {
        let mut end = max;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// Bytes from outside as text: valid UTF-8 kept as it is, anything else repaired.
pub fn from_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_text_is_repaired_and_good_text_left_alone() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT); CREATE TABLE w (k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;").unwrap();
        // (a broken byte, an encoded surrogate Rust refuses, good text with accents)
        conn.execute("INSERT INTO t (a, b) VALUES (CAST(?1 AS TEXT), 'Café ✓')", params![b"Bad \x80 byte".to_vec()]).unwrap();
        conn.execute("INSERT INTO t (a, b) VALUES (CAST(?1 AS TEXT), 'plain')", params![b"Surrogate \xED\xA0\x80 here".to_vec()]).unwrap();
        conn.execute("INSERT INTO w (k, v) VALUES ('x', CAST(?1 AS TEXT))", params![b"cut \xE2\x80".to_vec()]).unwrap();
        let done = repair(&conn).unwrap();
        assert_eq!(done.iter().map(|(_, _, n)| n).sum::<u64>(), 3, "{done:?}");
        let a: Vec<String> = conn.prepare("SELECT a FROM t ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(a[0], "Bad \u{FFFD} byte");
        assert!(a[1].starts_with("Surrogate ") && a[1].ends_with(" here"));
        let b: String = conn.query_row("SELECT b FROM t WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(b, "Café ✓");
        let v: String = conn.query_row("SELECT v FROM w", [], |r| r.get(0)).unwrap();
        assert!(v.starts_with("cut "));
        // nothing left to repair
        assert!(repair(&conn).unwrap().is_empty());
    }

    #[test]
    fn outside_text_is_cleaned_and_bounded() {
        assert_eq!(clean("a\0b", 10), "ab");
        assert_eq!(clean("ééé", 3), "é"); // (never cut inside a character)
        assert_eq!(from_bytes(b"ok \xFF"), "ok \u{FFFD}");
    }
}
