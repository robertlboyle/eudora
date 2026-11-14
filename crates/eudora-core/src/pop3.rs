//! POP3 (RFC 1939, STLS from RFC 2595): enough to read a mailbox. Messages are left on the
//! server unless the account says to delete them some days after Eudora has them
//! (`ImapConfig::pop_delete_after_days`: DELE, carried out at QUIT). A message's id is its UIDL.

use crate::error::{Error, Result};
use crate::mailnet::{Conn, Engine, TlsLevel};

pub struct Pop3 {
    conn: Conn,
}

fn answer(conn: &mut Conn, what: &str) -> Result<String> {
    let line = conn.line()?;
    if let Some(rest) = line.strip_prefix("+OK") {
        Ok(rest.trim().to_string())
    } else {
        let why = line.strip_prefix("-ERR").unwrap_or(&line).trim();
        Err(Error::Other(format!("{} said no to {what}: {why}", conn.host())))
    }
}

/// The server's words in a refusal ("… said no to the password: [AUTH] Authentication failed"
/// → "[AUTH] Authentication failed").
fn reason(refusal: &str) -> String {
    let words = refusal.rsplit_once(": ").map(|(_, r)| r).unwrap_or(refusal).trim();
    if words.is_empty() { "nothing more".to_string() } else { words.to_string() }
}

/// A refusal that says the mailbox is busy or the server can't now, not that the password is wrong.
fn busy(refusal: &str) -> bool {
    let r = refusal.to_ascii_lowercase();
    // ("account locked" after wrong passwords isn't one: trying again would only make it worse)
    ["[in-use]", "[login-delay]", "[sys/temp]", "in use", "mailbox locked", "maildrop locked", "mailbox is locked", "maildrop is locked", "try again later"]
        .iter()
        .any(|w| r.contains(w))
}

/// SASL PLAIN: "AUTH PLAIN", then the user and password, base64, after the server's "+".
fn auth_plain(conn: &mut Conn, user: &str, password: &str) -> Result<()> {
    use base64::Engine as _;
    conn.command("AUTH PLAIN")?;
    let line = conn.line()?;
    if !line.starts_with('+') || line.starts_with("+OK") {
        return Err(Error::Other(format!("AUTH PLAIN: {line}")));
    }
    let token = base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{password}"));
    conn.command(&token)?;
    answer(conn, "AUTH PLAIN").map(|_| ())
}

impl Pop3 {
    /// Connects and signs in. `security` "tls" (995), "starttls" (110 + STLS) or "none";
    /// encryption at `level`.
    pub(crate) fn open(host: &str, port: u16, security: &str, level: TlsLevel, user: &str, password: &str) -> Result<Pop3> {
        crate::mailnet::with_level(level, host, |engine| Pop3::open_with(host, port, security, engine, user, password))
    }

    fn open_with(host: &str, port: u16, security: &str, engine: Engine, user: &str, password: &str) -> Result<Pop3> {
        let mut conn = Conn::open(host, port, security == "tls", engine)?;
        answer(&mut conn, "connecting")?;
        if security == "starttls" {
            conn.command("STLS")?;
            answer(&mut conn, "a secure connection (STLS)")?;
            conn.start_tls(engine)?;
        }
        conn.command(&format!("USER {user}"))?;
        let signed_in = answer(&mut conn, "the user name").and_then(|_| {
            conn.command(&format!("PASS {password}"))?;
            answer(&mut conn, "the password")
        });
        let refused = match signed_in {
            Ok(_) => return Ok(Pop3 { conn }),
            Err(Error::Network(e)) => return Err(Error::Network(e)),
            Err(e) => e.to_string(),
        };
        // the mailbox held by another program (another computer's Eudora, a phone set to POP3):
        // RFC 2449's [IN-USE] / [LOGIN-DELAY] / [SYS/TEMP]: waited out like a network problem
        if busy(&refused) {
            return Err(Error::Network(format!(
                "{host}: the mailbox is in use by another program (another computer or phone checking it by POP3, or a check still finishing); Eudora tries again. The server said: {}",
                reason(&refused)
            )));
        }
        // a server that takes only SASL's AUTH PLAIN (RFC 5034), not USER / PASS
        if auth_plain(&mut conn, user, password).is_ok() {
            return Ok(Pop3 { conn });
        }
        Err(Error::Other(format!("{host} refused the user name or password. The server said: {}", reason(&refused))))
    }

    /// The encryption agreed with the server.
    pub(crate) fn security(&self) -> &str {
        self.conn.security()
    }

    /// Messages in the mailbox.
    pub(crate) fn count(&mut self) -> Result<u32> {
        self.conn.command("STAT")?;
        let stat = answer(&mut self.conn, "STAT")?;
        Ok(stat.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0))
    }

    /// (message number, UIDL) for every message, oldest first.
    /// Each message's lasting id: its UIDL, else (a server without UIDL, an optional command)
    /// "h-" and a hash of its header block (TOP n 0), which stays the same for the same message.
    pub(crate) fn uidl(&mut self) -> Result<Vec<(u32, String)>> {
        match self.uidl_only() {
            Err(Error::Other(_)) => self.ids_from_headers(),
            other => other,
        }
    }

    fn ids_from_headers(&mut self) -> Result<Vec<(u32, String)>> {
        let count = self.count()?;
        let mut out = Vec::with_capacity(count as usize);
        for n in 1..=count {
            self.conn.command(&format!("TOP {n} 0"))?;
            answer(&mut self.conn, &format!("reading message {n}'s headers (the server has no UIDL)"))?;
            let mut hasher = blake3::Hasher::new();
            loop {
                let mut line = Vec::new();
                self.conn.raw_line(&mut line)?;
                if line == b".\r\n" || line == b".\n" {
                    break;
                }
                hasher.update(&line);
            }
            out.push((n, format!("h-{}", &hasher.finalize().to_hex()[..32])));
        }
        Ok(out)
    }

    fn uidl_only(&mut self) -> Result<Vec<(u32, String)>> {
        self.conn.command("UIDL")?;
        answer(&mut self.conn, "listing the messages (UIDL)")?;
        let mut out = Vec::new();
        loop {
            let line = self.conn.line()?;
            if line == "." {
                break;
            }
            let mut parts = line.split_whitespace();
            if let (Some(n), Some(id)) = (parts.next().and_then(|n| n.parse().ok()), parts.next()) {
                out.push((n, id.to_string()));
            }
        }
        Ok(out)
    }

    /// (message number, size in bytes) for every message.
    pub(crate) fn sizes(&mut self) -> Result<Vec<(u32, u64)>> {
        self.conn.command("LIST")?;
        answer(&mut self.conn, "listing the messages (LIST)")?;
        let mut out = Vec::new();
        loop {
            let line = self.conn.line()?;
            if line == "." {
                break;
            }
            let mut parts = line.split_whitespace();
            if let (Some(n), Some(size)) = (parts.next().and_then(|n| n.parse().ok()), parts.next().and_then(|s| s.parse().ok())) {
                out.push((n, size));
            }
        }
        Ok(out)
    }

    /// Marks a message to be deleted (the server deletes it at QUIT).
    pub(crate) fn dele(&mut self, number: u32) -> Result<()> {
        self.conn.command(&format!("DELE {number}"))?;
        answer(&mut self.conn, &format!("deleting message {number}"))?;
        Ok(())
    }

    /// One message as stored (dot-unstuffed, line ends as sent).
    pub(crate) fn retr(&mut self, number: u32) -> Result<Vec<u8>> {
        self.conn.command(&format!("RETR {number}"))?;
        answer(&mut self.conn, &format!("reading message {number}"))?;
        let mut message = Vec::new();
        loop {
            let mut line = Vec::new();
            self.conn.raw_line(&mut line)?;
            if line == b".\r\n" || line == b".\n" {
                break;
            }
            if line.starts_with(b"..") {
                line.remove(0);
            }
            message.extend_from_slice(&line);
        }
        Ok(message)
    }

    pub(crate) fn quit(mut self) {
        let _ = self.conn.command("QUIT");
        let _ = self.conn.line();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    /// A POP3 server on this computer answering each line the client sends with `reply` (its
    /// answer's lines; empty = none), after a greeting. Returns its port.
    fn fake_server(reply: impl Fn(&str) -> Vec<String> + Send + 'static) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut out = stream.try_clone().unwrap();
            let _ = out.write_all(b"+OK ready\r\n");
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                for answer in reply(&line) {
                    let _ = out.write_all(format!("{answer}\r\n").as_bytes());
                }
                if line == "QUIT" {
                    break;
                }
            }
        });
        port
    }

    fn open(port: u16) -> Result<Pop3> {
        Pop3::open("127.0.0.1", port, "none", TlsLevel::High, "ann", "secret")
    }

    #[test]
    fn a_mailbox_in_use_is_said_and_waited_out_not_called_a_wrong_password() {
        let port = fake_server(|l| vec![if l.starts_with("PASS") { "-ERR [IN-USE] Mailbox locked by another session".into() } else { "+OK".into() }]);
        match open(port) {
            Err(Error::Network(e)) => assert!(e.contains("in use") && e.contains("[IN-USE]"), "{e}"),
            other => panic!("{:?}", other.err()),
        }
    }

    #[test]
    fn a_server_taking_only_auth_plain_is_signed_in_to() {
        use base64::Engine as _;
        let want = base64::engine::general_purpose::STANDARD.encode("\0ann\0secret");
        let port = fake_server(move |l| {
            vec![match l {
                l if l.starts_with("PASS") => "-ERR [AUTH] Plaintext authentication disallowed".into(),
                "AUTH PLAIN" => "+ ".into(),
                l if l == want => "+OK signed in".into(),
                _ => "+OK".into(),
            }]
        });
        assert!(open(port).is_ok());
    }

    #[test]
    fn a_wrong_password_says_so_with_the_server_s_own_words() {
        let port = fake_server(|l| {
            vec![match l {
                l if l.starts_with("PASS") => "-ERR [AUTH] Authentication failed.".into(),
                "AUTH PLAIN" => "-ERR Unknown command".into(),
                _ => "+OK".into(),
            }]
        });
        match open(port) {
            Err(Error::Other(e)) => assert!(e.contains("refused the user name or password") && e.contains("[AUTH] Authentication failed."), "{e}"),
            other => panic!("{:?}", other.err()),
        }
    }

    #[test]
    fn a_server_without_uidl_gets_ids_from_each_message_s_headers() {
        let port = fake_server(|l| match l {
            "UIDL" => vec!["-ERR Unknown command".into()],
            "STAT" => vec!["+OK 2 300".into()],
            "TOP 1 0" => vec!["+OK".into(), "Message-ID: <one@x>".into(), "Subject: One".into(), "".into(), ".".into()],
            "TOP 2 0" => vec!["+OK".into(), "Message-ID: <two@x>".into(), "Subject: Two".into(), "".into(), ".".into()],
            _ => vec!["+OK".into()],
        });
        let mut pop = open(port).unwrap();
        let ids = pop.uidl().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|(_, id)| id.starts_with("h-") && id.len() == 34));
        assert_ne!(ids[0].1, ids[1].1);
    }
}
