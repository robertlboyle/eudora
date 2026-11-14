//! Plugins (WebAssembly, decided 2026-10-16): a plugin is one WebAssembly file that adds
//! commands to Eudora's menus (and actions to its rules). It runs inside Eudora in a sandbox
//! (wasmtime) that gives it **nothing but Eudora's API, with only the permissions the user
//! granted it** (the same grant window, limits and log as any program): no files, no network,
//! no other programs, no clock to wait on. A plugin that asks for anything else is refused at
//! install. Each run is limited in memory (256 MB) and work (fuel: a runaway loop is stopped).
//! Unsigned plugins are allowed after a clear warning (2026-10-16). Format and an example:
//! docs/PLUGINS.md.
//!
//! **Not on iOS**: Apple allows no code made at
//! run time in App Store apps, which is how wasmtime runs a plugin; there installing or running
//! one says so (the list, switching off and removing still work).
//!
//! The file: a WebAssembly module with
//! - a custom section `eudora-plugin` holding its manifest (JSON: id, name, publisher, version,
//!   description, permissions, commands, rule_actions);
//! - exports `memory`, `alloc(len) -> ptr` and `run(ptr, len) -> packed` (input and output are
//!   JSON text in its memory; `packed` = ptr << 32 | len);
//! - imports only from the module `eudora`: `call(method_ptr, method_len, params_ptr,
//!   params_len) -> packed` (an API call, answered `{"result": …}` or `{"error": …}`) and
//!   `log(ptr, len)` (a line for the plugin's log).

use std::path::{Path, PathBuf};
#[cfg(not(target_os = "ios"))]
use std::sync::OnceLock;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(not(target_os = "ios"))]
use serde_json::json;
#[cfg(not(target_os = "ios"))]
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::api::Grant;
#[cfg(not(target_os = "ios"))]
use crate::api::Limits;
use crate::error::{Error, Result};
use crate::library::{Library, now};

#[cfg(not(target_os = "ios"))]
/// Memory a plugin may use while it runs.
const MEMORY: usize = 256 << 20;
#[cfg(not(target_os = "ios"))]
/// Work a run may do (wasmtime fuel: roughly instructions); seconds of computing, not hours.
const FUEL: u64 = 20_000_000_000;
#[cfg(not(target_os = "ios"))]
/// API calls a run may make.
const CALLS: u32 = 10_000;
/// The largest input or output passed between Eudora and a plugin.
#[cfg(not(target_os = "ios"))]
const MAX_TEXT: usize = 16 << 20;

/// What a plugin says about itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Its own id, unique (reversed domain: "com.example.invoices").
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// What it asks to be allowed (the user may grant less).
    #[serde(default)]
    pub permissions: Vec<Grant>,
    #[serde(default)]
    pub commands: Vec<Command>,
    /// Actions it offers to rules ("Run plugin …" on each message a rule matches).
    #[serde(default)]
    pub rule_actions: Vec<Command>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub id: String,
    pub title: String,
    /// Where it shows: "plugins" (Tools ▸ Plugins and the toolbar's Plugins menu), "message"
    /// (right-click on messages).
    #[serde(default)]
    pub r#where: Vec<String>,
}

/// An installed plugin.
#[derive(Clone, Debug, Serialize)]
pub struct Installed {
    pub manifest: Manifest,
    pub client: i64,
    pub enabled: bool,
    pub installed: i64,
    /// The user has answered its request (granted something).
    pub granted: bool,
}

/// What a run gave back.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Output {
    /// A line for the status bar.
    #[serde(default)]
    pub message: Option<String>,
    /// A page to show (in a sandboxed frame: no scripts).
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[cfg(not(target_os = "ios"))]
fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.consume_fuel(true);
        Engine::new(&config).expect("a WebAssembly engine")
    })
}

#[cfg(not(target_os = "ios"))]
/// The plugins' shared rate limits (each plugin has its own budget inside).
fn limits() -> &'static Limits {
    static LIMITS: OnceLock<Limits> = OnceLock::new();
    LIMITS.get_or_init(Limits::default)
}

#[cfg(not(target_os = "ios"))]
fn wasm_err(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("the plugin: {e}"))
}

/// The manifest inside a WebAssembly file (its custom section `eudora-plugin`).
pub fn read_manifest(bytes: &[u8]) -> Result<Manifest> {
    if bytes.len() < 8 || &bytes[0..4] != b"\0asm" {
        return Err(Error::Other("not a WebAssembly file".into()));
    }
    let leb = |pos: &mut usize| -> Option<usize> {
        let (mut value, mut shift) = (0usize, 0);
        loop {
            let byte = *bytes.get(*pos)?;
            *pos += 1;
            value |= ((byte & 0x7f) as usize) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
            if shift > 35 {
                return None;
            }
        }
    };
    let mut pos = 8;
    while pos < bytes.len() {
        let id = bytes[pos];
        pos += 1;
        let size = leb(&mut pos).ok_or_else(|| Error::Other("a damaged WebAssembly file".into()))?;
        let end = pos.checked_add(size).filter(|&e| e <= bytes.len()).ok_or_else(|| Error::Other("a damaged WebAssembly file".into()))?;
        if id == 0 {
            let mut p = pos;
            let name_len = leb(&mut p).ok_or_else(|| Error::Other("a damaged WebAssembly file".into()))?;
            let name = bytes.get(p..p + name_len).unwrap_or_default();
            if name == b"eudora-plugin" {
                let manifest: Manifest = serde_json::from_slice(&bytes[p + name_len..end]).map_err(|e| Error::Other(format!("its manifest: {e}")))?;
                check_manifest(&manifest)?;
                return Ok(manifest);
            }
        }
        pos = end;
    }
    Err(Error::Other("this WebAssembly file isn't an Eudora plugin (no eudora-plugin manifest)".into()))
}

fn check_manifest(m: &Manifest) -> Result<()> {
    let ok_id = !m.id.is_empty() && m.id.len() <= 100 && m.id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !ok_id {
        return Err(Error::Other("its manifest's id must be letters, digits, dots, dashes (com.example.name)".into()));
    }
    for (what, value, max) in [("name", &m.name, 80), ("publisher", &m.publisher, 80), ("version", &m.version, 40)] {
        if value.trim().is_empty() || value.len() > max {
            return Err(Error::Other(format!("its manifest's {what}: 1 to {max} characters")));
        }
    }
    if m.commands.is_empty() && m.rule_actions.is_empty() {
        return Err(Error::Other("it offers no command and no rule action".into()));
    }
    Ok(())
}

#[cfg(not(target_os = "ios"))]
/// Checks the module asks for nothing but Eudora's own two functions, and has what it must.
fn check_module(module: &Module) -> Result<()> {
    for import in module.imports() {
        if import.module() != "eudora" || !matches!(import.name(), "call" | "log") {
            return Err(Error::Other(format!(
                "it asks for something Eudora doesn't give plugins ({}.{}): plugins get only Eudora's API (no files, network or other programs)",
                import.module(),
                import.name()
            )));
        }
    }
    for needed in ["memory", "alloc", "run"] {
        if module.get_export(needed).is_none() {
            return Err(Error::Other(format!("it has no \"{needed}\" (docs/PLUGINS.md says what a plugin needs)")));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "ios"))]
/// What a running plugin may reach: the library through the API as its own client, nothing else.
struct Host {
    /// The library for the length of one run (`Library::plugin_run` borrows it for that long).
    library: *const Library,
    client: i64,
    plugin: String,
    calls: u32,
    limits: StoreLimits,
}

#[cfg(not(target_os = "ios"))]
// SAFETY: a Host lives only inside one `plugin_run` call, on its thread, while the &Library it
// points at is borrowed; wasmtime needs Send for its store data but the store never leaves it.
unsafe impl Send for Host {}

#[cfg(not(target_os = "ios"))]
fn packed(ptr: i32, len: usize) -> i64 {
    ((ptr as u32 as i64) << 32) | (len as u32 as i64)
}

#[cfg(not(target_os = "ios"))]
fn unpack(v: i64) -> (usize, usize) {
    (((v as u64) >> 32) as usize, (v as u64 & 0xffff_ffff) as usize)
}

#[cfg(not(target_os = "ios"))]
/// Copies text into the plugin's memory (through its own `alloc`).
fn give(caller: &mut Caller<'_, Host>, text: &str) -> wasmtime::Result<i64> {
    let alloc = caller.get_export("alloc").and_then(|e| e.into_func()).ok_or_else(|| wasmtime::Error::msg("no alloc"))?;
    let ptr = alloc.typed::<i32, i32>(&*caller)?.call(&mut *caller, text.len() as i32)?;
    let memory = caller.get_export("memory").and_then(|e| e.into_memory()).ok_or_else(|| wasmtime::Error::msg("no memory"))?;
    memory.write(&mut *caller, ptr as u32 as usize, text.as_bytes())?;
    Ok(packed(ptr, text.len()))
}

#[cfg(not(target_os = "ios"))]
fn take(caller: &mut Caller<'_, Host>, ptr: i32, len: i32) -> wasmtime::Result<String> {
    if len < 0 || len as usize > MAX_TEXT {
        return Err(wasmtime::Error::msg("text too long"));
    }
    let memory = caller.get_export("memory").and_then(|e| e.into_memory()).ok_or_else(|| wasmtime::Error::msg("no memory"))?;
    let mut buf = vec![0u8; len as usize];
    memory.read(&*caller, ptr as u32 as usize, &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Compiles the file and checks what it asks for (at install).
#[cfg(not(target_os = "ios"))]
fn compile_check(bytes: &[u8]) -> Result<()> {
    check_module(&Module::new(engine(), bytes).map_err(wasm_err)?)
}

#[cfg(target_os = "ios")]
fn compile_check(_bytes: &[u8]) -> Result<()> {
    Err(not_on_ios())
}

#[cfg(target_os = "ios")]
fn not_on_ios() -> Error {
    Error::Other("Plugins aren't available on iPhone and iPad yet: Apple doesn't let apps run them".into())
}

impl Library {
    fn plugins_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }

    /// Installs a plugin (Tools ▸ Plugins ▸ Install…): checks it, keeps a copy in the library,
    /// and asks the user for what it wants (the grant window). Returns its manifest. A plugin
    /// with the same id is replaced (an update: asked again, and its old grants go).
    pub fn plugin_install(&self, file: &Path) -> Result<Manifest> {
        let bytes = std::fs::read(file).map_err(|e| Error::Other(format!("reading {}: {e}", file.display())))?;
        let manifest = read_manifest(&bytes)?;
        compile_check(&bytes)?;
        std::fs::create_dir_all(self.plugins_dir()).map_err(|e| Error::Other(format!("making the plugins folder: {e}")))?;
        let name = format!("{}.wasm", manifest.id);
        std::fs::write(self.plugins_dir().join(&name), &bytes).map_err(|e| Error::Other(format!("keeping the plugin: {e}")))?;
        self.in_transaction(|| {
            if let Some(old) = self.conn.query_row("SELECT client FROM plugins WHERE id = ?1", params![manifest.id], |r| r.get::<_, i64>(0)).optional()? {
                self.api_revoke(old)?;
                self.conn.execute("DELETE FROM plugins WHERE id = ?1", params![manifest.id])?;
            }
            // its API client, and its request for the user (no secret: Eudora runs it)
            self.conn.execute(
                "INSERT INTO api_clients (name, publisher, kind, grants, created) VALUES (?1, ?2, 'plugin', '[]', ?3)",
                params![manifest.name, manifest.publisher, now()],
            )?;
            let client = self.conn.last_insert_rowid();
            let reason = if manifest.description.trim().is_empty() { format!("the plugin {} {}", manifest.name, manifest.version) } else { manifest.description.clone() };
            self.conn.execute(
                "INSERT INTO api_requests (client, secret_hash, reason, asked, created) VALUES (?1, x'00', ?2, ?3, ?4)",
                params![client, reason, serde_json::to_string(&manifest.permissions)?, now()],
            )?;
            self.conn.execute(
                "INSERT INTO plugins (id, client, file, hash, manifest, installed) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![manifest.id, client, name, blake3::hash(&bytes).as_bytes().to_vec(), serde_json::to_string(&manifest)?, now()],
            )?;
            Ok(())
        })?;
        Ok(manifest)
    }

    pub fn plugins(&self) -> Result<Vec<Installed>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.manifest, p.client, p.enabled, p.installed, (c.token_hash IS NOT NULL AND c.revoked IS NULL) FROM plugins p JOIN api_clients c ON c.id = p.client ORDER BY p.installed",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (manifest, client, enabled, installed, granted) = row?;
            out.push(Installed { manifest: serde_json::from_str(&manifest)?, client, enabled, installed, granted });
        }
        Ok(out)
    }

    pub fn plugin_enable(&self, id: &str, enabled: bool) -> Result<()> {
        self.conn.execute("UPDATE plugins SET enabled = ?2 WHERE id = ?1", params![id, enabled])?;
        Ok(())
    }

    /// Removes a plugin: its file, and every permission it had (revoked; its log stays).
    pub fn plugin_remove(&self, id: &str) -> Result<()> {
        let found: Option<(i64, String)> = self.conn.query_row("SELECT client, file FROM plugins WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((client, file)) = found else { return Ok(()) };
        self.api_revoke(client)?;
        self.conn.execute("DELETE FROM plugins WHERE id = ?1", params![id])?;
        let _ = std::fs::remove_file(self.plugins_dir().join(file));
        Ok(())
    }

    /// Runs one of a plugin's commands with `context` (the selection: {"ids": […], "folder": n},
    /// or {"message": n} for a rule action). The plugin sees only what the API lets it.
    pub fn plugin_run(&self, id: &str, command: &str, context: &Value) -> Result<Output> {
        let found: Option<(i64, String, Vec<u8>, String, bool)> = self
            .conn
            .query_row("SELECT client, file, hash, manifest, enabled FROM plugins WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
        let (client, file, hash, manifest, enabled) = found.ok_or_else(|| Error::Other(format!("no plugin {id} is installed")))?;
        if !enabled {
            return Err(Error::Other("that plugin is switched off (Tools ▸ Plugins)".into()));
        }
        let manifest: Manifest = serde_json::from_str(&manifest)?;
        if !manifest.commands.iter().chain(&manifest.rule_actions).any(|c| c.id == command) {
            return Err(Error::Other(format!("{} has no command {command}", manifest.name)));
        }
        let bytes = std::fs::read(self.plugins_dir().join(&file)).map_err(|e| Error::Other(format!("reading the plugin: {e}")))?;
        // a file changed since it was installed (by anything) never runs
        if blake3::hash(&bytes).as_bytes().as_slice() != hash.as_slice() {
            return Err(Error::Other(format!("{}'s file changed since it was installed: install it again", manifest.name)));
        }
        let started = std::time::Instant::now();
        let (result, calls) = self.execute(client, &manifest, &bytes, command, context);
        let detail = match &result {
            Ok(_) => format!("ran {command}: {calls} API calls, {:.1} s", started.elapsed().as_secs_f64()),
            Err(e) => format!("{command} failed: {e}"),
        };
        self.conn.execute(
            "INSERT INTO api_log (at, client, method, write, allowed, detail) VALUES (?1, ?2, 'plugin.run', 0, ?3, ?4)",
            params![now(), client, result.is_ok(), detail],
        )?;
        result
    }

    /// Compiles and runs the plugin's file (its run's result, and how many API calls it made).
    #[cfg(not(target_os = "ios"))]
    fn execute(&self, client: i64, manifest: &Manifest, bytes: &[u8], command: &str, context: &Value) -> (Result<Output>, u32) {
        let mut calls = 0;
        let result = (|| -> Result<Output> {
            let module = Module::new(engine(), bytes).map_err(wasm_err)?;
            check_module(&module)?;

            let host = Host { library: self as *const Library, client, plugin: manifest.id.clone(), calls: 0, limits: StoreLimitsBuilder::new().memory_size(MEMORY).instances(1).memories(1).tables(4).build() };
            let mut store = Store::new(engine(), host);
            store.limiter(|h| &mut h.limits);
            store.set_fuel(FUEL).map_err(wasm_err)?;
            let mut linker: Linker<Host> = Linker::new(engine());
            linker
                .func_wrap("eudora", "call", |mut caller: Caller<'_, Host>, mp: i32, ml: i32, pp: i32, pl: i32| -> wasmtime::Result<i64> {
                    let method = take(&mut caller, mp, ml)?;
                    let params_text = take(&mut caller, pp, pl)?;
                    let host = caller.data_mut();
                    host.calls += 1;
                    let answer = if host.calls > CALLS {
                        json!({ "error": { "code": -32002, "message": "too many calls in one run" } })
                    } else {
                        let params: Value = serde_json::from_str(&params_text).unwrap_or(json!({}));
                        // SAFETY: see `Host`: the library outlives the run
                        let library = unsafe { &*host.library };
                        match library.api_call_as(limits(), &crate::api::NoHost, host.client, &method, &params) {
                            Ok(result) => json!({ "result": result }),
                            Err(r) => json!({ "error": { "code": r.code, "message": r.message } }),
                        }
                    };
                    give(&mut caller, &answer.to_string())
                })
                .map_err(wasm_err)?;
            linker
                .func_wrap("eudora", "log", |mut caller: Caller<'_, Host>, p: i32, l: i32| -> wasmtime::Result<()> {
                    let line: String = take(&mut caller, p, l.min(2000))?.chars().take(500).collect();
                    let host = caller.data();
                    // SAFETY: see `Host`
                    let library = unsafe { &*host.library };
                    let _ = library.conn.execute(
                        "INSERT INTO api_log (at, client, method, write, allowed, detail) VALUES (?1, ?2, 'plugin.log', 0, 1, ?3)",
                        params![now(), host.client, format!("{}: {line}", host.plugin)],
                    );
                    Ok(())
                })
                .map_err(wasm_err)?;
            let instance = linker.instantiate(&mut store, &module).map_err(wasm_err)?;
            let run = instance.get_typed_func::<(i32, i32), i64>(&mut store, "run").map_err(wasm_err)?;
            let alloc = instance.get_typed_func::<i32, i32>(&mut store, "alloc").map_err(wasm_err)?;
            let memory = instance.get_memory(&mut store, "memory").ok_or_else(|| Error::Other("the plugin has no memory".into()))?;
            let input = json!({ "command": command, "context": context }).to_string();
            let ptr = alloc.call(&mut store, input.len() as i32).map_err(wasm_err)?;
            memory.write(&mut store, ptr as u32 as usize, input.as_bytes()).map_err(wasm_err)?;
            let outcome = run.call(&mut store, (ptr, input.len() as i32));
            calls = store.data().calls;
            let result = match outcome {
                Ok(v) => {
                    let (p, l) = unpack(v);
                    if l > MAX_TEXT {
                        Err(Error::Other("the plugin's answer is too long".into()))
                    } else {
                        let mut buf = vec![0u8; l];
                        memory.read(&store, p, &mut buf).map_err(wasm_err)?;
                        serde_json::from_slice::<Output>(&buf).map_err(|e| Error::Other(format!("the plugin's answer isn't what Eudora reads: {e}")))
                    }
                }
                Err(e) if store.get_fuel().map(|f| f == 0).unwrap_or(false) => Err(Error::Other(format!("{} worked too long and was stopped ({e})", manifest.name))),
                Err(e) => Err(wasm_err(e)),
            };
            result
        })();
        (result, calls)
    }

    #[cfg(target_os = "ios")]
    fn execute(&self, _client: i64, _manifest: &Manifest, _bytes: &[u8], _command: &str, _context: &Value) -> (Result<Output>, u32) {
        (Err(not_on_ios()), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Access;
    use crate::progress::Silent;
    use crate::{ImportOptions, import};

    /// A plugin as WebAssembly text with its manifest (built into a binary with the manifest
    /// section appended).
    fn plugin(manifest: &Value, body: &str) -> Vec<u8> {
        let mut wasm = wat::parse_str(body).unwrap();
        let text = manifest.to_string();
        let name = b"eudora-plugin";
        let mut content = vec![name.len() as u8];
        content.extend_from_slice(name);
        content.extend_from_slice(text.as_bytes());
        wasm.push(0); // a custom section
        let mut size = content.len();
        loop {
            let byte = (size & 0x7f) as u8;
            size >>= 7;
            wasm.push(if size > 0 { byte | 0x80 } else { byte });
            if size == 0 {
                break;
            }
        }
        wasm.extend_from_slice(&content);
        wasm
    }

    fn manifest(id: &str, perms: Value) -> Value {
        json!({ "id": id, "name": "Subject Reader", "publisher": "Test", "version": "1.0", "permissions": perms, "commands": [{ "id": "go", "title": "Go", "where": ["message"] }] })
    }

    /// A plugin that calls the API once with the method and params it holds, and returns
    /// `{"message": <the API's answer as text>}`.
    fn caller_plugin(method: &str, params: &str) -> String {
        let call = format!("{method}{params}");
        let (ml, pl) = (method.len(), params.len());
        let escaped: String = call.bytes().map(|b| format!("\\{b:02x}")).collect();
        format!(
            r#"(module
              (import "eudora" "call" (func $call (param i32 i32 i32 i32) (result i64)))
              (memory (export "memory") 2)
              (global $next (mut i32) (i32.const 4096))
              (data (i32.const 0) "{escaped}")
              (data (i32.const 1024) "{{\"message\":\"done\"}}")
              (func (export "alloc") (param $n i32) (result i32)
                (local $p i32)
                (local.set $p (global.get $next))
                (global.set $next (i32.add (global.get $next) (local.get $n)))
                (local.get $p))
              (func (export "run") (param i32 i32) (result i64)
                (local $answer i64)
                (local.set $answer (call $call (i32.const 0) (i32.const {ml}) (i32.const {ml}) (i32.const {pl})))
                ;; whatever the API answered: {{"message":"done"}} (18 bytes at 1024)
                (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const 18))))"#
        )
    }

    fn library() -> (tempfile::TempDir, Library, i64) {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let file = dir.path().join("1.eml");
        std::fs::write(&file, "From: ann@a.com\r\nTo: me@x.com\r\nSubject: Invoice 42\r\nMessage-ID: <1@a>\r\n\r\nPay me\r\n").unwrap();
        import(&mut library, &[file], &ImportOptions { destination: Some("Inbox".into()), ..Default::default() }, &Silent).unwrap();
        let id: i64 = library.conn.query_row("SELECT id FROM messages", [], |r| r.get(0)).unwrap();
        (dir, library, id)
    }

    fn install(library: &Library, dir: &Path, name: &str, bytes: &[u8]) -> Result<Manifest> {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        library.plugin_install(&path)
    }

    #[test]
    fn a_plugin_gets_only_what_it_was_granted() {
        let (dir, library, id) = library();
        let wasm = plugin(&manifest("test.subject", json!([{ "permission": "mail.headers.read" }])), &caller_plugin("v1.mail.headers", &format!("{{\"id\":{id}}}")));
        let m = install(&library, dir.path(), "p.wasm", &wasm).unwrap();
        assert_eq!(m.id, "test.subject");
        let client = library.plugins().unwrap()[0].client;
        // before the user answers: its call is refused (and logged)
        library.plugin_run("test.subject", "go", &json!({})).unwrap();
        let log = library.api_log(Some(client), 50).unwrap();
        assert!(log.iter().any(|l| l.method == "v1.mail.headers" && !l.allowed), "{log:?}");
        // the user grants headers: the call goes through, and is logged as this plugin's
        let request = library.api_requests().unwrap()[0].id;
        library.api_answer(request, Some(&[Grant { permission: crate::api::Permission::HeadersRead, accounts: vec![], folders: vec![] }])).unwrap();
        library.plugin_run("test.subject", "go", &json!({})).unwrap();
        let log = library.api_log(Some(client), 50).unwrap();
        assert!(log.iter().any(|l| l.method == "v1.mail.headers" && l.allowed), "{log:?}");
        // a body it wasn't granted: refused
        let reader = plugin(&manifest("test.body", json!([{ "permission": "mail.bodies.read" }])), &caller_plugin("v1.mail.body", &format!("{{\"id\":{id}}}")));
        install(&library, dir.path(), "b.wasm", &reader).unwrap();
        let request = library.api_requests().unwrap()[0].id;
        library.api_answer(request, Some(&[Grant { permission: crate::api::Permission::HeadersRead, accounts: vec![], folders: vec![] }])).unwrap();
        let _ = library.plugin_run("test.body", "go", &json!({}));
        let body_client = library.plugins().unwrap().iter().find(|p| p.manifest.id == "test.body").unwrap().client;
        assert!(library.api_log(Some(body_client), 50).unwrap().iter().any(|l| l.method == "v1.mail.body" && !l.allowed));
    }

    /// Writes a sample plugin (to try the window: `EUDORA_SAMPLE_PLUGIN=path cargo test …
    /// write_sample_plugin -- --ignored`): it asks to see accounts, lists them once, and says hello.
    #[test]
    #[ignore]
    fn write_sample_plugin() {
        let path = std::env::var("EUDORA_SAMPLE_PLUGIN").expect("EUDORA_SAMPLE_PLUGIN=where to write it");
        let m = json!({
            "id": "example.hello", "name": "Hello Plugin", "publisher": "Eudora examples", "version": "1.0",
            "description": "Says hello and counts your accounts (a sample)",
            "permissions": [{ "permission": "accounts.read" }],
            "commands": [{ "id": "go", "title": "Say Hello", "where": ["plugins", "message"] }],
        });
        let body = caller_plugin("v1.accounts.list", "{}").replace(r#"{\"message\":\"done\"}"#, r#"{\"message\":\"hi!\"}"#).replace("(i64.const 18)", "(i64.const 17)");
        std::fs::write(path, plugin(&m, &body)).unwrap();
    }

    #[test]
    fn a_plugin_asking_for_files_or_network_is_refused() {
        let (dir, library, _) = library();
        let wasi = plugin(
            &manifest("test.wasi", json!([])),
            r#"(module
                (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) (i32.const 0))
                (func (export "run") (param i32 i32) (result i64) (i64.const 0)))"#,
        );
        let e = install(&library, dir.path(), "w.wasm", &wasi).unwrap_err().to_string();
        assert!(e.contains("doesn't give plugins"), "{e}");
        assert!(library.plugins().unwrap().is_empty());
        // and a WebAssembly file that isn't a plugin
        let plain = wat::parse_str("(module)").unwrap();
        assert!(install(&library, dir.path(), "x.wasm", &plain).unwrap_err().to_string().contains("isn't an Eudora plugin"));
    }

    #[test]
    fn a_runaway_plugin_is_stopped_and_a_changed_file_never_runs() {
        let (dir, library, _) = library();
        let looping = plugin(
            &manifest("test.loop", json!([])),
            r#"(module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) (i32.const 0))
                (func (export "run") (param i32 i32) (result i64) (loop $l (br $l)) (i64.const 0)))"#,
        );
        install(&library, dir.path(), "l.wasm", &looping).unwrap();
        let e = library.plugin_run("test.loop", "go", &json!({})).unwrap_err().to_string();
        assert!(e.contains("worked too long"), "{e}");
        // its file changed afterwards (by anything): refused
        let path = library.root.join("plugins").join("test.loop.wasm");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0);
        std::fs::write(&path, bytes).unwrap();
        assert!(library.plugin_run("test.loop", "go", &json!({})).unwrap_err().to_string().contains("changed since it was installed"));
        // removed: its permissions go too
        library.plugin_remove("test.loop").unwrap();
        assert!(library.plugins().unwrap().is_empty());
    }
}
