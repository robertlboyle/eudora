//! A connection to a mail server (IMAP, POP3, SMTP): plain TCP that is either TLS from the
//! start or switched to TLS by STARTTLS / STLS, at the encryption level the account allows
//! (`TlsLevel`). Certificates are always checked the way Windows (or the platform) checks
//! them. Network trouble comes back as `Error::Network` (worth waiting and trying again);
//! a server that can't agree on encryption at the level allowed, as `Error::Tls`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use rustls_platform_verifier::BuilderVerifierExt;

use crate::error::{Error, Result};

/// How long a server may stay silent before the connection counts as lost.
const TIMEOUT: Duration = Duration::from_secs(90);

/// How strong the encryption must be. Stored by name in the account's settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TlsLevel {
    /// TLS 1.3 only.
    Strict,
    /// TLS 1.3, and TLS 1.2 with current ciphers only (forward-secret key exchange, AEAD:
    /// AES-GCM, ChaCha20-Poly1305). The default.
    #[default]
    High,
    /// High first; if the server can't agree, the system's own TLS (Windows SChannel, macOS
    /// Security, OpenSSL on Linux), which also accepts older TLS 1.2 ciphers (and TLS 1.0 / 1.1
    /// where the system still allows them).
    Compatible,
}

impl TlsLevel {
    pub fn from_name(name: &str) -> TlsLevel {
        match name.to_ascii_lowercase().as_str() {
            "strict" => TlsLevel::Strict,
            "compatible" => TlsLevel::Compatible,
            _ => TlsLevel::High,
        }
    }

    fn engines(self) -> &'static [Engine] {
        match self {
            TlsLevel::Strict => &[Engine::Tls13],
            TlsLevel::High => &[Engine::Current],
            TlsLevel::Compatible => &[Engine::Current, Engine::System],
        }
    }
}

/// One way of making the TLS connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Engine {
    Tls13,
    Current,
    System,
}

/// Runs `f` (a whole sign-in, since a failed handshake uses up the connection) with each
/// engine the level allows, stopping at the first that doesn't fail on encryption.
pub(crate) fn with_level<T>(level: TlsLevel, host: &str, mut f: impl FnMut(Engine) -> Result<T>) -> Result<T> {
    let mut why = String::new();
    for engine in level.engines() {
        match f(*engine) {
            Err(Error::Tls(e)) => why = e,
            other => return other,
        }
    }
    let advice = match level {
        TlsLevel::Strict => "The server may not offer TLS 1.3: set Encryption to High to also allow current TLS 1.2.",
        TlsLevel::High => "The server may only offer older encryption: set Encryption to Compatible to allow it for this server.",
        TlsLevel::Compatible => "Even older encryption didn't work: the server may need settings this computer no longer allows.",
    };
    Err(Error::Tls(format!("{host} and Eudora couldn't agree on encryption ({why}). {advice}")))
}

enum Stream {
    Plain(TcpStream),
    Rustls(Box<StreamOwned<ClientConnection, TcpStream>>),
    #[cfg(not(target_os = "android"))]
    System(Box<native_tls::TlsStream<TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Rustls(s) => &s.sock,
            #[cfg(not(target_os = "android"))]
            Stream::System(s) => s.get_ref(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Rustls(s) => s.read(buf),
            #[cfg(not(target_os = "android"))]
            Stream::System(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Rustls(s) => s.write(buf),
            #[cfg(not(target_os = "android"))]
            Stream::System(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Rustls(s) => s.flush(),
            #[cfg(not(target_os = "android"))]
            Stream::System(s) => s.flush(),
        }
    }
}

/// The connection handed to the IMAP library (it reads and writes it itself).
pub struct ImapStream(Stream);

impl Read for ImapStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for ImapStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl imap::extensions::idle::SetReadTimeout for ImapStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> imap::Result<()> {
        self.0.tcp().set_read_timeout(timeout).map_err(imap::Error::Io)
    }
}

pub(crate) struct Conn {
    host: String,
    reader: Option<BufReader<Stream>>,
    /// What encryption was agreed ("TLS 1.3, TLS13_AES_256_GCM_SHA384"), or "not encrypted".
    security: String,
}

pub(crate) fn network(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Network(format!("{what}: {e}"))
}

fn rustls_config(engine: Engine) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider);
    let builder = if engine == Engine::Tls13 {
        builder.with_protocol_versions(&[&rustls::version::TLS13])
    } else {
        // (rustls offers only forward-secret AEAD suites for TLS 1.2: no CBC, no RSA key exchange)
        builder.with_safe_default_protocol_versions()
    }
    .map_err(|e| Error::Other(format!("TLS: {e}")))?;
    let config = builder.with_platform_verifier().map_err(|e| Error::Other(format!("TLS: {e}")))?.with_no_client_auth();
    Ok(Arc::new(config))
}

fn is_certificate_problem(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("certificate") || t.contains("cert ") || t.contains("unknownissuer") || t.contains("trust")
}

/// A handshake failure: a certificate problem stays an error of its own (never retried with
/// weaker settings); a broken connection is the network; anything else is "couldn't agree".
fn handshake_error(host: &str, e: impl std::fmt::Display, io: Option<std::io::ErrorKind>) -> Error {
    let text = e.to_string();
    if is_certificate_problem(&text) {
        return Error::Other(format!("{host}'s security certificate wasn't accepted ({text})"));
    }
    match io {
        Some(std::io::ErrorKind::TimedOut | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::WouldBlock)
            if !text.contains("alert") =>
        {
            network(&format!("a secure connection to {host}"), text)
        }
        _ => Error::Tls(text),
    }
}

fn version_name(v: Option<rustls::ProtocolVersion>) -> String {
    match v {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLS 1.3".into(),
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLS 1.2".into(),
        Some(other) => format!("{other:?}"),
        None => "TLS".into(),
    }
}

fn wrap(host: &str, tcp: TcpStream, engine: Engine) -> Result<(Stream, String)> {
    match engine {
        Engine::Tls13 | Engine::Current => {
            let name = ServerName::try_from(host.to_string()).map_err(|e| Error::Other(format!("{host}: {e}")))?;
            let conn = ClientConnection::new(rustls_config(engine)?, name).map_err(|e| Error::Other(format!("TLS: {e}")))?;
            let mut stream = StreamOwned::new(conn, tcp);
            // finish the handshake now, so a problem shows here and not on the first read
            while stream.conn.is_handshaking() {
                if let Err(e) = stream.conn.complete_io(&mut stream.sock) {
                    let kind = e.kind();
                    return Err(handshake_error(host, e, Some(kind)));
                }
            }
            let suite = stream.conn.negotiated_cipher_suite().map(|s| format!("{:?}", s.suite())).unwrap_or_default();
            let security = format!("{}, {suite}", version_name(stream.conn.protocol_version()));
            Ok((Stream::Rustls(Box::new(stream)), security))
        }
        Engine::System => system_tls(host, tcp),
    }
}

/// The system's own TLS (Windows SChannel, macOS Security, OpenSSL on Linux), which also takes
/// older ciphers and protocol versions where the system still allows them.
#[cfg(not(target_os = "android"))]
fn system_tls(host: &str, tcp: TcpStream) -> Result<(Stream, String)> {
    let connector = native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv10))
        .build()
        .map_err(|e| Error::Other(format!("the system's TLS: {e}")))?;
    match connector.connect(host, tcp) {
        Ok(stream) => Ok((Stream::System(Box::new(stream)), "the system's TLS, older encryption allowed (Compatible)".into())),
        Err(native_tls::HandshakeError::Failure(e)) => Err(handshake_error(host, &e, None)),
        Err(native_tls::HandshakeError::WouldBlock(_)) => Err(Error::Network(format!("a secure connection to {host} timed out"))),
    }
}

/// Android has no system TLS a program can use (no OpenSSL): only rustls's High and Strict.
#[cfg(target_os = "android")]
fn system_tls(host: &str, _tcp: TcpStream) -> Result<(Stream, String)> {
    Err(Error::Tls(format!("{host} needs older encryption (Compatible), which Eudora on Android doesn't have: only High and Strict")))
}

impl Conn {
    /// Connects; `tls` = TLS from the start (ports 993, 995, 465), with `engine`.
    pub(crate) fn open(host: &str, port: u16, tls: bool, engine: Engine) -> Result<Conn> {
        let addresses: Vec<_> = (host, port).to_socket_addrs().map_err(|e| network(&format!("finding {host}"), e))?.collect();
        let mut last = None;
        let mut tcp = None;
        for a in addresses {
            match TcpStream::connect_timeout(&a, Duration::from_secs(30)) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let tcp = match (tcp, last) {
            (Some(t), _) => t,
            (None, Some(e)) => return Err(network(&format!("connecting to {host}:{port}"), e)),
            (None, None) => return Err(Error::Network(format!("{host} has no address"))),
        };
        tcp.set_read_timeout(Some(TIMEOUT)).ok();
        tcp.set_write_timeout(Some(TIMEOUT)).ok();
        let (stream, security) = if tls { wrap(host, tcp, engine)? } else { (Stream::Plain(tcp), "not encrypted".to_string()) };
        Ok(Conn { host: host.to_string(), reader: Some(BufReader::new(stream)), security })
    }

    /// Switches the connection to TLS (after the server agreed to STARTTLS / STLS).
    pub(crate) fn start_tls(&mut self, engine: Engine) -> Result<()> {
        let reader = self.reader.take().expect("open connection");
        match reader.into_inner() {
            Stream::Plain(tcp) => {
                let (stream, security) = wrap(&self.host, tcp, engine)?;
                self.reader = Some(BufReader::new(stream));
                self.security = security;
            }
            secure => self.reader = Some(BufReader::new(secure)),
        }
        Ok(())
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// The encryption agreed with the server.
    pub(crate) fn security(&self) -> &str {
        &self.security
    }

    /// The connection for the IMAP library. Anything already read and not used is lost, so
    /// call it right after connecting or after STARTTLS.
    pub(crate) fn into_imap(mut self) -> (ImapStream, String) {
        let stream = self.reader.take().expect("open connection").into_inner();
        (ImapStream(stream), self.security)
    }

    fn reader(&mut self) -> &mut BufReader<Stream> {
        self.reader.as_mut().expect("open connection")
    }

    /// One line from the server, without its line end.
    pub(crate) fn line(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        let n = self.reader().read_until(b'\n', &mut buf).map_err(|e| network("reading from the server", e))?;
        if n == 0 {
            return Err(Error::Network("the server closed the connection".into()));
        }
        while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            buf.pop();
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// One line as bytes, line end kept (message text).
    pub(crate) fn raw_line(&mut self, buf: &mut Vec<u8>) -> Result<usize> {
        let n = self.reader().read_until(b'\n', buf).map_err(|e| network("reading from the server", e))?;
        if n == 0 {
            return Err(Error::Network("the server closed the connection".into()));
        }
        Ok(n)
    }

    pub(crate) fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let stream = self.reader().get_mut();
        stream.write_all(bytes).and_then(|_| stream.flush()).map_err(|e| network("writing to the server", e))
    }

    /// Sends a command line (CRLF added).
    pub(crate) fn command(&mut self, line: &str) -> Result<()> {
        self.send(format!("{line}\r\n").as_bytes())
    }
}

/// Text sent over SMTP / read from POP3 ends lines with CRLF and doubles a leading dot.
pub(crate) fn dot_stuffed(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + message.len() / 50 + 5);
    let mut at_line_start = true;
    let mut i = 0;
    while i < message.len() {
        let b = message[i];
        if at_line_start && b == b'.' {
            out.push(b'.');
        }
        match b {
            b'\r' if message.get(i + 1) == Some(&b'\n') => {
                out.extend_from_slice(b"\r\n");
                i += 2;
                at_line_start = true;
                continue;
            }
            b'\n' | b'\r' => {
                out.extend_from_slice(b"\r\n");
                at_line_start = true;
            }
            _ => {
                out.push(b);
                at_line_start = false;
            }
        }
        i += 1;
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_and_line_ends() {
        assert_eq!(dot_stuffed(b"a\n.b\r\n..c"), b"a\r\n..b\r\n...c\r\n");
        assert_eq!(dot_stuffed(b".\r\n"), b"..\r\n");
        assert_eq!(dot_stuffed(b"x\ry"), b"x\r\ny\r\n");
    }

    #[test]
    fn levels_try_weaker_encryption_only_when_allowed() {
        assert_eq!(TlsLevel::from_name(""), TlsLevel::High);
        assert_eq!(TlsLevel::from_name("Strict").engines(), &[Engine::Tls13]);
        assert_eq!(TlsLevel::High.engines(), &[Engine::Current]);
        assert_eq!(TlsLevel::Compatible.engines(), &[Engine::Current, Engine::System]);
        // High: a handshake failure is reported (with advice), never retried weaker
        let mut tried = Vec::new();
        let r: Result<()> = with_level(TlsLevel::High, "h", |e| {
            tried.push(e);
            Err(Error::Tls("no shared cipher".into()))
        });
        assert_eq!(tried, vec![Engine::Current]);
        assert!(matches!(r, Err(Error::Tls(m)) if m.contains("Compatible")));
        // Compatible: falls back once, only on encryption failures
        let mut tried = Vec::new();
        let r: Result<u8> = with_level(TlsLevel::Compatible, "h", |e| {
            tried.push(e);
            if e == Engine::Current { Err(Error::Tls("no shared cipher".into())) } else { Ok(7) }
        });
        assert_eq!((tried.len(), r.unwrap()), (2, 7));
        // a network failure is never a reason to weaken encryption
        let mut tried = 0;
        let r: Result<()> = with_level(TlsLevel::Compatible, "h", |_| {
            tried += 1;
            Err(Error::Network("down".into()))
        });
        assert_eq!(tried, 1);
        assert!(matches!(r, Err(Error::Network(_))));
        // nor is a certificate problem
        assert!(matches!(handshake_error("h", "invalid peer certificate: UnknownIssuer", None), Error::Other(_)));
    }
}
