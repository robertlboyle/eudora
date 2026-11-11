//! The local API's transport: JSON-RPC 2.0, one request per line, over a **named pipe** on
//! Windows (`\\.\pipe\eudora27-<user>`) and a **Unix socket** elsewhere
//! (`<library>/api.sock`, readable by the user only). No network port: other computers and
//! web pages can't reach it.
//!
//! On Windows the pipe admits only the user who started Eudora (its security descriptor gives
//! the owner alone access; remote clients are refused by the pipe itself; a second copy can't
//! take the name over: the first instance is created with FILE_FLAG_FIRST_PIPE_INSTANCE).
//!
//! A request: `{"jsonrpc": "2.0", "id": 1, "method": "v1.mail.search", "params": {...},
//! "token": "eud1_…"}` (the token from `v1.auth.collect`; none to ask for access). Every call
//! goes through `Library::api_call`: permissions, rate limits and the log are all there.

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::{Value, json};

use crate::api::Refusal;
use crate::error::{Error, Result};

/// Answers one call: (token, method, params).
pub type Handler = Arc<dyn Fn(Option<&str>, &str, &Value) -> std::result::Result<Value, Refusal> + Send + Sync>;

/// The longest request accepted (a message being sent with attachments).
const MAX_LINE: usize = 32 << 20;
/// Connections at once.
const MAX_CONNECTIONS: usize = 16;

/// Where clients connect (Windows: the pipe's name; elsewhere: the socket's path).
pub fn address(root: &std::path::Path) -> String {
    #[cfg(windows)]
    {
        let _ = root;
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        let clean: String = user.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
        format!(r"\\.\pipe\eudora27-{clean}")
    }
    #[cfg(not(windows))]
    {
        root.join("api.sock").display().to_string()
    }
}

/// Answers one line of JSON-RPC.
pub fn answer_line(handler: &Handler, line: &str) -> String {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("not JSON: {e}") } }).to_string(),
    };
    let id = request["id"].clone();
    let Some(method) = request["method"].as_str() else {
        return json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32600, "message": "no method" } }).to_string();
    };
    let params = if request["params"].is_null() { json!({}) } else { request["params"].clone() };
    match handler(request["token"].as_str(), method, &params) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
        Err(r) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": r.code, "message": r.message } }).to_string(),
    }
}

/// Serves one connection until the client goes (or sends something too long).
fn serve_connection<S: std::io::Read + Write>(stream: S, handler: &Handler, stop: &AtomicBool) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let mut limited = (&mut reader).take(MAX_LINE as u64 + 1);
        match limited.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(n) if n > MAX_LINE => {
                let _ = writeln!(reader.get_mut(), "{}", json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "request too long" } }));
                return;
            }
            Ok(_) => {}
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if line.trim().is_empty() {
            continue;
        }
        let answer = answer_line(handler, line.trim());
        if writeln!(reader.get_mut(), "{answer}").is_err() || reader.get_mut().flush().is_err() {
            return;
        }
    }
}

use std::io::Read;

/// Serves until `stop` is set. Blocks: run it on a thread of its own.
pub fn serve(root: &std::path::Path, handler: Handler, stop: Arc<AtomicBool>) -> Result<()> {
    system::serve(&address(root), handler, stop)
}

/// Wakes a running `serve` so it sees `stop` (connects once and goes).
pub fn wake(root: &std::path::Path) {
    system::wake(&address(root));
}

#[cfg(windows)]
mod system {
    use std::fs::File;
    use std::os::windows::io::FromRawHandle;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};
    use windows::core::HSTRING;

    use super::{Handler, MAX_CONNECTIONS, serve_connection};
    use crate::error::{Error, Result};

    /// Only the pipe's owner (the user running Eudora) may open it; nobody else, not even
    /// other users of this computer. ("OW": owner rights; "P": not inherited from anywhere.)
    const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)";

    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }
    unsafe impl Send for Descriptor {}

    fn create(name: &str, first: bool, sd: &Descriptor) -> Result<HANDLE> {
        let attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0.0, bInheritHandle: false.into() };
        let mut mode: FILE_FLAGS_AND_ATTRIBUTES = PIPE_ACCESS_DUPLEX;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let handle = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(name),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                Some(&attributes),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let e = windows::core::Error::from_thread();
            return Err(Error::Other(if first {
                format!("the API's pipe {name} is in use (another copy of Eudora serves it?): {e}")
            } else {
                format!("the API's pipe: {e}")
            }));
        }
        Ok(handle)
    }

    pub fn serve(name: &str, handler: Handler, stop: Arc<AtomicBool>) -> Result<()> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(OWNER_ONLY), SDDL_REVISION_1, &mut sd, None) }
            .map_err(|e| Error::Other(format!("the API's pipe security: {e}")))?;
        let sd = Descriptor(sd);
        let open = Arc::new(AtomicUsize::new(0));
        let mut first = true;
        while !stop.load(Ordering::SeqCst) {
            let handle = create(name, first, &sd)?;
            first = false;
            let connected = unsafe { ConnectNamedPipe(handle, None) };
            // (a client that connected between create and connect counts as connected)
            if connected.as_ref().is_err_and(|e| e.code() != windows::Win32::Foundation::ERROR_PIPE_CONNECTED.to_hresult()) {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                continue;
            }
            if stop.load(Ordering::SeqCst) {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                break;
            }
            // the File owns the handle from here (closed when the connection ends)
            let file = unsafe { File::from_raw_handle(handle.0) };
            if open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                drop(file); // too many at once: this one is turned away
                continue;
            }
            open.fetch_add(1, Ordering::SeqCst);
            let (handler, stop, open) = (handler.clone(), stop.clone(), open.clone());
            std::thread::spawn(move || {
                serve_connection(file, &handler, &stop);
                open.fetch_sub(1, Ordering::SeqCst);
            });
        }
        Ok(())
    }

    pub fn wake(name: &str) {
        let _ = std::fs::OpenOptions::new().read(true).write(true).open(name);
    }
}

#[cfg(not(windows))]
mod system {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::{Handler, MAX_CONNECTIONS, serve_connection};
    use crate::error::{Error, Result};

    pub fn serve(path: &str, handler: Handler, stop: Arc<AtomicBool>) -> Result<()> {
        // a socket left by a copy that ended badly: gone unless something answers on it
        if UnixStream::connect(path).is_ok() {
            return Err(Error::Other(format!("the API's socket {path} is in use (another copy of Eudora serves it?)")));
        }
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).map_err(|e| Error::Other(format!("the API's socket {path}: {e}")))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| Error::Other(format!("the API's socket: {e}")))?;
        let open = Arc::new(AtomicUsize::new(0));
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            if open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                continue;
            }
            open.fetch_add(1, Ordering::SeqCst);
            let (handler, stop, open) = (handler.clone(), stop.clone(), open.clone());
            std::thread::spawn(move || {
                serve_connection(stream, &handler, &stop);
                open.fetch_sub(1, Ordering::SeqCst);
            });
        }
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    pub fn wake(path: &str) {
        let _ = UnixStream::connect(path);
    }
}

/// (keeps the imports the systems share in use everywhere)
#[allow(dead_code)]
fn _shared(_: AtomicUsize, _: Error) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_lines_are_answered() {
        let handler: Handler = Arc::new(|token, method, params| {
            if method == "echo" { Ok(json!({ "token": token, "params": params })) } else { Err(Refusal { code: -32601, message: "no such method".into() }) }
        });
        let ok: Value = serde_json::from_str(&answer_line(&handler, r#"{"jsonrpc":"2.0","id":7,"method":"echo","params":{"a":1},"token":"t"}"#)).unwrap();
        assert_eq!(ok["id"], 7);
        assert_eq!(ok["result"]["params"]["a"], 1);
        assert_eq!(ok["result"]["token"], "t");
        let bad: Value = serde_json::from_str(&answer_line(&handler, r#"{"id":8,"method":"nope"}"#)).unwrap();
        assert_eq!(bad["error"]["code"], -32601);
        let garbage: Value = serde_json::from_str(&answer_line(&handler, "{not json")).unwrap();
        assert_eq!(garbage["error"]["code"], -32700);
    }

    #[test]
    fn the_pipe_serves_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let handler: Handler = Arc::new(|_, method, _| Ok(json!({ "method": method })));
        let stop = Arc::new(AtomicBool::new(false));
        // (a test pipe of its own, so a running Eudora isn't disturbed)
        let name = if cfg!(windows) { format!(r"\\.\pipe\eudora27-test-{}", std::process::id()) } else { dir.path().join("t.sock").display().to_string() };
        let (n, h, s) = (name.clone(), handler.clone(), stop.clone());
        let server = std::thread::spawn(move || system::serve(&n, h, s));
        std::thread::sleep(std::time::Duration::from_millis(300));
        #[cfg(windows)]
        let mut client = std::fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
        #[cfg(not(windows))]
        let mut client = std::os::unix::net::UnixStream::connect(&name).unwrap();
        writeln!(client, r#"{{"jsonrpc":"2.0","id":1,"method":"v1.api.methods"}}"#).unwrap();
        let mut answer = String::new();
        BufReader::new(&mut client).read_line(&mut answer).unwrap();
        assert!(answer.contains("v1.api.methods"), "{answer}");
        drop(client);
        stop.store(true, Ordering::SeqCst);
        system::wake(&name);
        server.join().unwrap().unwrap();
    }
}
