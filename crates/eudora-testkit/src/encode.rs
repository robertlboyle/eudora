//! Encoders the generator needs (base64, uuencode, encoded words, Windows-1252), written out so
//! the test kit doesn't depend on the code it tests.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 in 76-character lines.
pub fn base64_lines(data: &[u8], eol: &str) -> String {
    let mut out = String::with_capacity(data.len() * 4 / 3 + data.len() / 57 * 2 + 4);
    for line in data.chunks(57) {
        for chunk in line.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            out.push(B64[(n >> 18) as usize & 63] as char);
            out.push(B64[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
        }
        out.push_str(eol);
    }
    out
}

/// A uuencoded block ("begin 644 name" … "end").
pub fn uuencode(name: &str, data: &[u8], eol: &str) -> String {
    let enc = |v: u8| if v == 0 { '`' } else { (v + 32) as char };
    let mut out = format!("begin 644 {name}{eol}");
    for line in data.chunks(45) {
        out.push((line.len() as u8 + 32) as char);
        for chunk in line.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            out.push(enc(b[0] >> 2));
            out.push(enc(((b[0] << 4) | (b[1] >> 4)) & 63));
            out.push(enc(((b[1] << 2) | (b[2] >> 6)) & 63));
            out.push(enc(b[2] & 63));
        }
        out.push_str(eol);
    }
    out.push('`');
    out.push_str(eol);
    out.push_str("end");
    out.push_str(eol);
    out
}

/// "=?utf-8?B?…?=" for a header with non-ASCII text.
pub fn encoded_word(text: &str) -> String {
    if text.is_ascii() {
        return text.to_string();
    }
    let b = base64_lines(text.as_bytes(), "");
    format!("=?utf-8?B?{b}?=")
}

/// Text as Windows-1252 bytes (characters it can't hold become '?').
pub fn windows_1252(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| match c {
            '\u{20ac}' => 0x80,
            '\u{2018}' => 0x91,
            '\u{2019}' => 0x92,
            '\u{201c}' => 0x93,
            '\u{201d}' => 0x94,
            '\u{2013}' => 0x96,
            '\u{2014}' => 0x97,
            c if (c as u32) < 0x100 => c as u32 as u8,
            _ => b'?',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_values() {
        assert_eq!(base64_lines(b"%PDF-1.4\n", ""), "JVBERi0xLjQK");
        assert_eq!(base64_lines(b"ab", ""), "YWI=");
    }

    #[test]
    fn uuencode_known_value() {
        assert_eq!(uuencode("cat.txt", b"Cat", "\n"), "begin 644 cat.txt\n#0V%T\n`\nend\n");
    }
}
