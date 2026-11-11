//! Sending through an SMTP server (RFC 5321) with STARTTLS (RFC 3207) or TLS from the start
//! (port 465), signing in with AUTH PLAIN or LOGIN (RFC 4954). The message goes as built; its
//! Bcc header is left out of what's sent (the recipients get it through RCPT only).

use base64::Engine as _;

use crate::error::{Error, Result};
use crate::mailnet::{Conn, Engine, TlsLevel, dot_stuffed};

pub struct SmtpSettings<'a> {
    pub host: &'a str,
    pub port: u16,
    /// "tls" (465), "starttls" (587 / 25) or "none".
    pub security: &'a str,
    /// Empty: no sign-in (a server that relays for this computer).
    pub user: &'a str,
    pub password: &'a str,
    pub level: TlsLevel,
}

/// A reply: its code and its text (all lines).
fn reply(conn: &mut Conn) -> Result<(u16, String)> {
    let mut text = Vec::new();
    loop {
        let line = conn.line()?;
        let code = line.get(..3).and_then(|c| c.parse::<u16>().ok()).ok_or_else(|| Error::Other(format!("{} sent an odd answer: {line}", conn.host())))?;
        let more = line.as_bytes().get(3) == Some(&b'-');
        text.push(line.get(4..).unwrap_or("").to_string());
        if !more {
            return Ok((code, text.join("\n")));
        }
    }
}

fn expect(conn: &mut Conn, ok: &[u16], what: &str) -> Result<String> {
    let (code, text) = reply(conn)?;
    if ok.contains(&code) {
        Ok(text)
    } else {
        Err(Error::Other(format!("{} refused {what}: {code} {text}", conn.host())))
    }
}

fn hello(conn: &mut Conn) -> Result<String> {
    let me = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "eudora".into()).to_lowercase();
    conn.command(&format!("EHLO {me}"))?;
    expect(conn, &[250], "the greeting (EHLO)")
}

/// Connects, says hello, secures the connection and signs in: ready for MAIL FROM.
fn session(s: &SmtpSettings) -> Result<Conn> {
    crate::mailnet::with_level(s.level, s.host, |engine| session_with(s, engine))
}

fn session_with(s: &SmtpSettings, engine: Engine) -> Result<Conn> {
    let mut conn = Conn::open(s.host, s.port, s.security == "tls", engine)?;
    expect(&mut conn, &[220], "the connection")?;
    let mut caps = hello(&mut conn)?;
    if s.security == "starttls" {
        if !caps.lines().any(|l| l.trim().eq_ignore_ascii_case("STARTTLS")) {
            return Err(Error::Other(format!("{} doesn't offer STARTTLS on port {}: try SSL/TLS (port 465)", s.host, s.port)));
        }
        conn.command("STARTTLS")?;
        expect(&mut conn, &[220], "a secure connection (STARTTLS)")?;
        conn.start_tls(engine)?;
        caps = hello(&mut conn)?;
    }
    if !s.user.is_empty() {
        let auth = caps.lines().find_map(|l| l.strip_prefix("AUTH ").or_else(|| l.strip_prefix("AUTH="))).unwrap_or("").to_uppercase();
        let b64 = |t: &str| base64::engine::general_purpose::STANDARD.encode(t);
        let signed_in = if auth.split_whitespace().any(|m| m == "PLAIN") || !auth.contains("LOGIN") {
            conn.command(&format!("AUTH PLAIN {}", b64(&format!("\0{}\0{}", s.user, s.password))))?;
            reply(&mut conn)?
        } else {
            conn.command("AUTH LOGIN")?;
            expect(&mut conn, &[334], "signing in (AUTH LOGIN)")?;
            conn.command(&b64(s.user))?;
            expect(&mut conn, &[334], "the user name")?;
            conn.command(&b64(s.password))?;
            reply(&mut conn)?
        };
        if signed_in.0 != 235 {
            return Err(Error::Other(format!("{} refused the user name or password ({} {})", s.host, signed_in.0, signed_in.1)));
        }
    }
    Ok(conn)
}

/// Signs in and out without sending anything (the account window's Test). Returns the
/// encryption agreed.
pub fn test(s: &SmtpSettings) -> Result<String> {
    let mut conn = session(s)?;
    let security = conn.security().to_string();
    let _ = conn.command("QUIT");
    Ok(security)
}

/// Sends `message` from `from` to every address in `to`.
pub fn send(s: &SmtpSettings, from: &str, to: &[String], message: &[u8]) -> Result<()> {
    let mut conn = session(s)?;
    conn.command(&format!("MAIL FROM:<{from}>"))?;
    expect(&mut conn, &[250], &format!("the sender {from}"))?;
    let mut refused = Vec::new();
    for address in to {
        conn.command(&format!("RCPT TO:<{address}>"))?;
        let (code, text) = reply(&mut conn)?;
        if !(250..=251).contains(&code) {
            refused.push(format!("{address} ({code} {text})"));
        }
    }
    if refused.len() == to.len() {
        let _ = conn.command("QUIT");
        return Err(Error::Other(format!("{} refused every recipient: {}", s.host, refused.join("; "))));
    }
    conn.command("DATA")?;
    expect(&mut conn, &[354], "the message (DATA)")?;
    conn.send(&dot_stuffed(&without_bcc(message)))?;
    conn.send(b".\r\n")?;
    expect(&mut conn, &[250], "the message")?;
    let _ = conn.command("QUIT");
    if !refused.is_empty() {
        return Err(Error::Other(format!("Sent, but {} refused: {}", s.host, refused.join("; "))));
    }
    Ok(())
}

/// The message without its Bcc header (and the header's folded lines).
pub fn without_bcc(message: &[u8]) -> Vec<u8> {
    let end = memchr::memmem::find(message, b"\r\n\r\n").map(|i| i + 2).or_else(|| memchr::memmem::find(message, b"\n\n").map(|i| i + 1)).unwrap_or(message.len());
    let (head, body) = message.split_at(end);
    let mut out = Vec::with_capacity(message.len());
    let mut skipping = false;
    for line in head.split_inclusive(|b| *b == b'\n') {
        let folded = line.first().is_some_and(|b| *b == b' ' || *b == b'\t');
        if !folded {
            skipping = line.len() >= 4 && line[..4].eq_ignore_ascii_case(b"bcc:");
        }
        if !skipping {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcc_is_left_out_of_what_is_sent() {
        let m = b"From: a@x\r\nBcc: b@y,\r\n c@z\r\nTo: d@w\r\nSubject: s\r\n\r\nBcc: stays in the body\r\n";
        assert_eq!(without_bcc(m), b"From: a@x\r\nTo: d@w\r\nSubject: s\r\n\r\nBcc: stays in the body\r\n".to_vec());
    }
}
