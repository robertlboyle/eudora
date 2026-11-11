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
        if let Err(e) = signed_in {
            return Err(match e {
                Error::Network(_) => e,
                _ => Error::Other(format!("{host} refused the user name or password")),
            });
        }
        Ok(Pop3 { conn })
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
    pub(crate) fn uidl(&mut self) -> Result<Vec<(u32, String)>> {
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
