//! Text from inside attachments: PDFs (their own text), and later pictures (OCR). The text
//! is kept per message (`extracted`), read for amounts (`links`) and added to what search
//! finds.

use std::panic::{AssertUnwindSafe, catch_unwind};

use mail_parser::{MessageParser, MimeHeaders};

/// The largest PDF read, and the most pages read from one.
const PDF_LIMIT: usize = 20 << 20;
const PDF_PAGES: u32 = 30;
/// The most text kept from one attachment.
const TEXT_LIMIT: usize = 100_000;

/// A message's PDF attachments as text: (its part number, its text). A PDF that can't be read
/// (damaged, encrypted, odd) gives nothing, never an error.
pub fn pdf_texts(raw: &[u8]) -> Vec<(String, String)> {
    let Some(message) = catch_unwind(AssertUnwindSafe(|| MessageParser::default().parse(raw))).ok().flatten() else { return Vec::new() };
    let mut out = Vec::new();
    for (n, part) in message.parts.iter().enumerate() {
        let is_pdf = part.content_type().is_some_and(|ct| ct.ctype().eq_ignore_ascii_case("application") && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case("pdf")))
            || part.attachment_name().is_some_and(|name| name.to_lowercase().ends_with(".pdf"));
        if !is_pdf {
            continue;
        }
        let bytes = part.contents();
        if bytes.len() > PDF_LIMIT || !bytes.starts_with(b"%PDF") {
            continue;
        }
        if let Some(text) = pdf_text(bytes) {
            if !text.trim().is_empty() {
                out.push((n.to_string(), text));
            }
        }
    }
    out
}

/// The most pictures read from one message.
const PICTURES: usize = 12;

/// A message's pictures read by OCR: the ones in it (attached or inline), and, when `web` is
/// true (the sender is one the user always shows pictures from), the ones its HTML loads from
/// the web. Returns (where: "img:N" / "web:N", the text).
pub fn picture_texts(raw: &[u8], web: bool) -> Vec<(String, String)> {
    let Some(message) = catch_unwind(AssertUnwindSafe(|| MessageParser::default().parse(raw))).ok().flatten() else { return Vec::new() };
    let mut out = Vec::new();
    let mut htmls = Vec::new();
    for (n, part) in message.parts.iter().enumerate() {
        if let mail_parser::PartType::Html(h) = &part.body {
            htmls.push(h.to_string());
            continue;
        }
        let is_picture = part.content_type().is_some_and(|ct| ct.ctype().eq_ignore_ascii_case("image"));
        if !is_picture || out.len() >= PICTURES {
            continue;
        }
        if let Some(text) = crate::ocr::picture_text(part.contents()) {
            out.push((format!("img:{n}"), text));
        }
    }
    if web {
        // (only for senders the user always shows pictures from: fetching tells the sender)
        let urls: Vec<String> = htmls.iter().flat_map(|h| crate::remote_images::find(h)).take(PICTURES).collect();
        for (n, picture) in crate::remote_images::fetch(&urls, None).into_iter().enumerate() {
            let Some(data) = picture.data else { continue };
            let Some(b64) = data.split_once(";base64,").map(|(_, b)| b) else { continue };
            use base64::Engine as _;
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else { continue };
            if let Some(text) = crate::ocr::picture_text(&bytes) {
                out.push((format!("web:{n}"), text));
            }
        }
    }
    out
}

/// One PDF's text (its first `PDF_PAGES` pages).
pub fn pdf_text(bytes: &[u8]) -> Option<String> {
    catch_unwind(AssertUnwindSafe(|| {
        let doc = lopdf::Document::load_mem(bytes).ok()?;
        let pages: Vec<u32> = doc.get_pages().keys().copied().filter(|p| *p <= PDF_PAGES).collect();
        let mut text = doc.extract_text(&pages).ok()?;
        if text.len() > TEXT_LIMIT {
            let mut cut = TEXT_LIMIT;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        Some(text)
    }))
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-page PDF saying `text`, made with lopdf.
    fn pdf(text: &str) -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier" });
        let resources_id = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font_id } });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => content_id });
        doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1, "Resources" => resources_id, "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()] }));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn a_pdf_attachments_text_is_read() {
        use base64::Engine as _;
        let body = base64::engine::general_purpose::STANDARD.encode(pdf("Invoice total: $123.45"));
        let raw = format!(
            "From: billing@acme.example\r\nSubject: Your invoice\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--b\r\nContent-Type: application/pdf; name=invoice.pdf\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=invoice.pdf\r\n\r\n{body}\r\n--b--\r\n"
        );
        let texts = pdf_texts(raw.as_bytes());
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].1.contains("Invoice total: $123.45"), "{:?}", texts[0].1);
        assert_eq!(crate::links::amounts(&texts[0].1), vec![12345]);
        // not a PDF, or a damaged one: nothing, no error
        assert!(pdf_text(b"%PDF-1.5 broken").is_none());
    }
}
