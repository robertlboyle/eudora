//! Payments linked to their receipts.
//!
//! Each message in Payments Sent and Receipts is read once for the dollar amounts in its
//! subject and text (`amounts`; a message with none gets a row of -1, so it isn't read again);
//! then a payment and a receipt sharing an amount within 30 minutes are linked (`links`).

use rusqlite::params;
use serde::Serialize;

use crate::error::Result;
use crate::library::Library;

/// How far apart a payment and its receipt may be, either first.
pub const LINK_WINDOW: i64 = 1800;

/// Amounts in a text, in cents: with a currency ("$1,234.50", "€2,500.00", "£9.99", "USD 12.00",
/// "12.00 EUR"), or with two decimals next to a word that says it's money ("Total: 490.00",
/// "490.00 TOTAL", "Amount 45.00"). Numbers without either (order numbers, dates) aren't amounts.
pub fn amounts(text: &str) -> Vec<i64> {
    const SYMBOLS: [&str; 4] = ["$", "\u{20ac}", "\u{a3}", "\u{a5}"];
    const CODES: [&str; 6] = ["USD", "EUR", "GBP", "CAD", "AUD", "CHF"];
    const WORDS: [&str; 11] = ["total", "amount", "charged", "paid", "price", "balance", "due", "payment", "subtotal", "sum", "cost"];
    let b = text.as_bytes();
    // a number at i: (cents, its end, whether it had two decimals)
    let number_at = |mut i: usize| -> Option<(i64, usize, bool)> {
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        let start = i;
        let mut whole: i64 = 0;
        let mut digits = 0;
        while i < b.len() && (b[i].is_ascii_digit() || (b[i] == b',' && digits > 0 && b.get(i + 1..i + 4).is_some_and(|d| d.iter().all(u8::is_ascii_digit)))) {
            if b[i].is_ascii_digit() {
                whole = whole.checked_mul(10)?.checked_add((b[i] - b'0') as i64)?;
                digits += 1;
            }
            i += 1;
        }
        if digits == 0 || digits > 9 {
            return None;
        }
        let mut cents = 0;
        let mut decimals = false;
        if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) && b.get(i + 2).is_some_and(u8::is_ascii_digit) && !b.get(i + 3).is_some_and(u8::is_ascii_digit) {
            cents = ((b[i + 1] - b'0') * 10 + (b[i + 2] - b'0')) as i64;
            i += 3;
            decimals = true;
        }
        (i > start).then_some((whole * 100 + cents, i, decimals))
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let rest = &text[i..];
        // a currency before: "$45", "€2,500.00", "USD 12.00"
        if let Some(sym) = SYMBOLS.iter().chain(CODES.iter()).find(|s| rest.starts_with(**s)) {
            if let Some((c, end, _)) = number_at(i + sym.len()) {
                out.push(c);
                i = end;
                continue;
            }
        }
        let starts_number = b[i].is_ascii_digit() && (i == 0 || (!b[i - 1].is_ascii_alphanumeric() && b[i - 1] != b'.' && b[i - 1] != b','));
        if starts_number {
            if let Some((c, end, decimals)) = number_at(i) {
                // a currency after ("12.00 USD"), or two decimals beside a money word
                // the words around it, from the text itself (byte positions on whole characters:
                // lowercasing or a non-breaking space must not cut one)
                let from = text[..i].char_indices().rev().nth(29).map_or(0, |(k, _)| k);
                let to = text[end..].char_indices().nth(24).map_or(text.len(), |(k, _)| end + k);
                let before = text[from..i].to_lowercase();
                let after_text = text[end..to].to_lowercase();
                let after = after_text.trim_start();
                let currency_after = CODES.iter().any(|c| after.starts_with(&c.to_lowercase()));
                let word_near = decimals && (WORDS.iter().any(|w| before.contains(w)) || WORDS.iter().any(|w| after.starts_with(w)));
                if currency_after || word_near {
                    out.push(c);
                }
                i = end.max(i + 1);
                continue;
            }
        }
        i += 1;
    }
    out.retain(|c| *c > 0);
    out.sort_unstable();
    out.dedup();
    out
}

/// Raised when `amounts` reads more: everything is read again (and linked again).
pub const AMOUNTS_VERSION: &str = "6";

/// A message linked to another.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Linked {
    pub id: i64,
    /// "receipt" (for a payment) or "payment" (for a receipt).
    pub kind: String,
    pub subject: String,
    pub from: String,
    pub date: i64,
    /// The amount they share, in cents.
    pub cents: i64,
}

impl Library {
    /// Reads up to `limit` payments and receipts not read yet for their amounts, then links
    /// what matches. Returns how many were read (0: all done).
    pub fn link_payments(&self, limit: usize) -> Result<u64> {
        if self.setting("amounts-version")?.as_deref() != Some(AMOUNTS_VERSION) {
            self.conn.execute_batch("DELETE FROM amounts; DELETE FROM links;")?;
            self.set_setting("amounts-version", AMOUNTS_VERSION)?;
        }
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT t.message, m.subject, m.pack, m.pack_offset, m.pack_len, COALESCE(ad.address, '') FROM tags t JOIN messages m ON m.id = t.message
             LEFT JOIN message_addresses ma ON ma.message = m.id AND ma.role = 0 AND ma.position = 0 LEFT JOIN addresses ad ON ad.id = ma.address
             WHERE t.tag IN ('payments-sent', 'receipts') AND NOT EXISTS (SELECT 1 FROM amounts a WHERE a.message = t.message) ORDER BY t.message DESC LIMIT ?1",
        )?;
        let rows: Vec<(i64, String, crate::pack::Location, String)> =
            stmt.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?, Library::location(r, 2)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
        // senders the user always shows pictures from (Settings ▸ Mail): only theirs are
        // fetched from the web to be read
        let always: Vec<String> = self
            .setting("pictures")?
            .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
            .and_then(|v| v["senders"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_lowercase)).collect()))
            .unwrap_or_default();
        if rows.is_empty() {
            return Ok(0);
        }
        // read in parallel (the packs), written by this thread
        use rayon::prelude::*;
        let packs = &self.packs;
        // (the message's text, its PDFs' text, and when that has no amount, its pictures read by
        // OCR: receipts and invoices often come as a PDF or a picture)
        let found: Vec<(i64, Vec<i64>, Vec<(String, String, &'static str)>)> = rows
            .par_iter()
            .map(|(id, subject, location, from)| {
                let raw = packs.read(*location).map(|r| r.raw).unwrap_or_default();
                let text = crate::parse::all_text(&raw);
                let mut extracted: Vec<(String, String, &'static str)> = crate::extract::pdf_texts(&raw).into_iter().map(|(p, t)| (p, t, "pdf")).collect();
                let mut all: String = text.chars().take(20_000).collect();
                for (_, t, _) in &extracted {
                    all.push('\n');
                    all.extend(t.chars().take(20_000));
                }
                let mut cents = amounts(&format!("{subject}\n{all}"));
                if cents.is_empty() {
                    let web = !from.is_empty() && always.contains(&from.to_lowercase());
                    for (p, t) in crate::extract::picture_texts(&raw, web) {
                        cents.extend(amounts(&t));
                        extracted.push((p, t, "ocr"));
                    }
                    cents.sort_unstable();
                    cents.dedup();
                }
                (*id, cents, extracted)
            })
            .collect();
        let mut keep = self.conn.prepare_cached("INSERT OR REPLACE INTO extracted (message, part, source, text) VALUES (?1, ?2, ?3, ?4)")?;
        let mut reindex = self.conn.prepare_cached("INSERT OR IGNORE INTO index_pending (message) VALUES (?1)")?;
        let mut insert = self.conn.prepare_cached("INSERT OR IGNORE INTO amounts (message, cents) VALUES (?1, ?2)")?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            for (id, cents, pdfs) in &found {
                // the PDFs' text: kept, and found by search once the message is indexed again
                for (part, text, source) in pdfs {
                    keep.execute(params![id, part, source, text])?;
                }
                if !pdfs.is_empty() {
                    reindex.execute(params![id])?;
                }
                if cents.is_empty() {
                    insert.execute(params![id, -1])?;
                }
                for c in cents {
                    insert.execute(params![id, c])?;
                }
            }
            // payments read in this batch, and payments whose receipt was read in it
            let ids: Vec<String> = found.iter().map(|(id, _, _)| id.to_string()).collect();
            let list = ids.join(",");
            self.conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO links (payment, receipt, cents)
                     SELECT p.message, r.message, pa.cents FROM tags p JOIN amounts pa ON pa.message = p.message
                     JOIN tags r ON r.tag = 'receipts' AND r.date BETWEEN p.date - {LINK_WINDOW} AND p.date + {LINK_WINDOW} AND r.message <> p.message
                     JOIN amounts ra ON ra.message = r.message AND ra.cents = pa.cents
                     WHERE p.tag = 'payments-sent' AND pa.cents > 0 AND (p.message IN ({list}) OR r.message IN ({list}))"
                ),
                [],
            )?;
            Ok(())
        })();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result?;
        Ok(rows.len() as u64)
    }

    /// The messages linked to this one (its receipt, or its payment).
    pub fn links_of(&self, id: i64) -> Result<Vec<Linked>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT l.receipt, 'receipt', l.cents FROM links l WHERE l.payment = ?1 UNION SELECT l.payment, 'payment', l.cents FROM links l WHERE l.receipt = ?1",
        )?;
        let pairs: Vec<(i64, String, i64)> = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for (other, kind, cents) in pairs {
            let (subject, from, date): (String, String, i64) =
                self.conn.query_row("SELECT subject, from_display, date FROM messages WHERE id = ?1", params![other], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            out.push(Linked { id: other, kind, subject, from, date, cents });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollar_amounts_are_found() {
        assert_eq!(amounts("Total: $1,234.50 (tax $4.05)"), vec![405, 123_450]);
        assert_eq!(amounts("You sent $45 USD to Shop"), vec![4500]);
        assert_eq!(amounts("Amount: USD 12.00, ref 2026-10-14"), vec![1200]);
        assert_eq!(amounts("Charged 19.99 USD on card 4471"), vec![1999]);
        assert_eq!(amounts("Order #11234 shipped on 10/14"), Vec::<i64>::new(), "numbers without a currency aren't amounts");
        assert_eq!(amounts("$0.00 due"), Vec::<i64>::new());
        assert_eq!(amounts("Line Item  Qty  Price  Total\n   490.00 SUB-TOTAL\n   490.00 TOTAL"), vec![49_000]);
        assert_eq!(amounts("Amount: \u{20ac}2,500.00 EUR"), vec![250_000]);
        assert_eq!(amounts("Order Amount: 59.98"), vec![5998]);
        assert_eq!(amounts("Invoice 2026.10 sent on 10.14.2026"), Vec::<i64>::new(), "no money word: not amounts");
        // non-breaking spaces, and letters that change length when lowercased (HTML receipts)
        let nbsp = "\u{a0}".repeat(20);
        assert_eq!(amounts(&format!("Total{nbsp}: 12.50{nbsp}")), vec![1250]);
        assert_eq!(amounts(&format!("{} amount 9.00", "\u{130}".repeat(20))), vec![900]);
    }

    #[test]
    fn a_payment_and_its_receipt_ten_minutes_apart_are_linked() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |n: u32, from: &str, subject: &str, time: &str, body: &str| {
            let p = src.join(format!("{n}.eml"));
            std::fs::write(&p, format!("From: {from}\r\nSubject: {subject}\r\nDate: Mon, 13 Oct 2026 {time} +0000\r\nMessage-ID: <{n}@x>\r\n\r\n{body}\r\n")).unwrap();
            p
        };
        let files = vec![
            mail(1, "service@paypal.com", "You sent a payment", "10:00:00", "You sent $45.00 USD to Shop"),
            mail(2, "orders@shop.example", "Your order receipt", "10:04:00", "Order total: $45.00"),
            mail(3, "orders@other.example", "Your order receipt", "10:05:00", "Order total: $12.00"),
            mail(4, "orders@late.example", "Your order receipt", "10:45:00", "Order total: $45.00"),
        ];
        crate::import(&mut library, &files, &crate::ImportOptions::default(), &Silent).unwrap();
        while library.tag_views(100).unwrap() > 0 {}
        assert_eq!(library.count_view("payments-sent", &[]).unwrap(), 1);
        while library.link_payments(2).unwrap() > 0 {}
        let id = |n: &str| -> i64 { library.conn.query_row("SELECT id FROM messages WHERE message_id = ?1", [n], |r| r.get(0)).unwrap() };
        let links = library.links_of(id("1@x")).unwrap();
        assert_eq!(links.iter().map(|l| (l.id, l.kind.as_str(), l.cents)).collect::<Vec<_>>(), vec![(id("2@x"), "receipt", 4500)], "same amount, 4 minutes: linked; 45 minutes or another amount: not");
        assert_eq!(library.links_of(id("2@x")).unwrap()[0].id, id("1@x"), "and the other way round");
    }
}
