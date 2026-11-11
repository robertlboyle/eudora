# Writing an Eudora plugin

A plugin adds commands to Eudora: in Tools ▸ Plugins, in the right-click menu on messages, and as an action rules can run. It's **one WebAssembly file** (`.wasm`) that Eudora runs sealed off:
- It can reach nothing but **Eudora's API**, with only the permissions the user granted it in Eudora's grant window. That means no files, no network, no other programs and no clock.
- Every API call it makes is checked, rate-limited and **logged** like any program's (Preferences ▸ Programs and plugins ▸ Log).
- Each run may use at most 256 MB of memory and a bounded amount of work; a runaway loop is stopped.
- The file is checked against its checksum before every run; a file changed after installing never runs.

Plugins aren't signed yet: Eudora shows a warning before installing one.

## The file

A WebAssembly module (`wasm32-unknown-unknown`, no WASI) with:

| | |
|---|---|
| custom section `eudora-plugin` | the manifest (JSON, below) |
| export `memory` | its memory |
| export `alloc(len: i32) -> i32` | gives Eudora room for `len` bytes (Eudora writes inputs and API answers there) |
| export `run(ptr: i32, len: i32) -> i64` | runs a command: the input is JSON at `ptr`; it returns `ptr << 32 \| len` of its output (JSON) |
| import `eudora.call(method_ptr, method_len, params_ptr, params_len) -> i64` | an API call: the method's name and its params (JSON); returns `ptr << 32 \| len` of the answer, `{"result": …}` or `{"error": {"code", "message"}}` |
| import `eudora.log(ptr, len)` | a line for the plugin's log |

**Anything else it imports, and it's refused at install.**

### The manifest

```json
{
  "id": "com.example.invoices",
  "name": "Invoice Finder",
  "publisher": "Example Ltd",
  "version": "1.0",
  "description": "Lists the invoice numbers in the selected messages",
  "permissions": [
    { "permission": "mail.headers.read" },
    { "permission": "mail.bodies.read", "folders": ["Inbox"] }
  ],
  "commands": [ { "id": "list", "title": "List Invoice Numbers", "where": ["plugins", "message"] } ],
  "rule_actions": [ { "id": "tag", "title": "Label with Invoice" } ]
}
```

- **`permissions`** are what it asks for: `accounts.read`, `mail.headers.read`, `mail.bodies.read`, `mail.attachments.read`, `mail.search`, `mail.change`, `mail.delete`, `mail.send` (each message shown to the user first, unless they allow all from it), `rules.read`, `rules.write`. Each can be limited to `accounts` (addresses) and `folders`. The user may grant less.
- **`where`** a command shows: `plugins` (Tools ▸ Plugins), `message` (the right-click menu on messages).

### Input and output
- **Input** to `run`: `{"command": "list", "context": {"ids": [12, 13], "folder": 4}}`. For a rule action: `{"command": "tag", "context": {"message": 12, "folder": 4, "rule": "Invoices"}}`.
- **Output**: `{"message": "3 invoices: 1001, 1002, 1007"}` (shown in the status bar), and / or `{"html": "<table>…</table>"}` (shown in a window, without scripts or web pictures), or `{"error": "…"}`.

### The API
The same methods as for programs: `eudora-cli api methods` lists them with their permissions and parameters. For example:
- `v1.mail.headers {id}`
- `v1.mail.body {id}`
- `v1.mail.search {query}`
- `v1.mail.mark {ids, label}`
- `v1.mail.send {draft}`

## In Rust

```rust
// Cargo.toml: [lib] crate-type = ["cdylib"]; dependencies: serde_json
// Build: cargo build --release --target wasm32-unknown-unknown
// Then add the manifest section, e.g. with `wasm-tools metadata add` or a build script:
//   the custom section "eudora-plugin" holding manifest.json.

#[link(wasm_import_module = "eudora")]
extern "C" {
    fn call(method: *const u8, method_len: usize, params: *const u8, params_len: usize) -> u64;
    fn log(text: *const u8, len: usize);
}

#[no_mangle]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

fn text(packed: u64) -> String {
    let (ptr, len) = ((packed >> 32) as *const u8, (packed & 0xffff_ffff) as usize);
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) }).into_owned()
}

fn api(method: &str, params: serde_json::Value) -> serde_json::Value {
    let p = params.to_string();
    serde_json::from_str(&text(unsafe { call(method.as_ptr(), method.len(), p.as_ptr(), p.len()) })).unwrap_or_default()
}

fn give(out: serde_json::Value) -> u64 {
    let s = out.to_string().into_bytes().leak();
    ((s.as_ptr() as u64) << 32) | s.len() as u64
}

#[no_mangle]
pub extern "C" fn run(ptr: *const u8, len: usize) -> u64 {
    let input: serde_json::Value = serde_json::from_slice(unsafe { std::slice::from_raw_parts(ptr, len) }).unwrap_or_default();
    let mut found = Vec::new();
    for id in input["context"]["ids"].as_array().into_iter().flatten() {
        let body = api("v1.mail.body", serde_json::json!({ "id": id }));
        if let Some(text) = body["result"]["text"].as_str() {
            for word in text.split_whitespace().filter(|w| w.starts_with("INV-")) {
                found.push(word.to_string());
            }
        }
    }
    let note = format!("checked {} messages", found.len());
    unsafe { log(note.as_ptr(), note.len()) };
    give(serde_json::json!({ "message": format!("{} invoices: {}", found.len(), found.join(", ")) }))
}
```

## Installing and trying it
1. In Eudora: **Tools ▸ Plugins… ▸ Install a plugin…**.
2. Choose the `.wasm`; Eudora warns that it can't check who made it.
3. Tick what the plugin may do in the grant window that follows.
4. Then select some messages and run its command (Tools ▸ Plugins, or right-click).

From the command line: `eudora-cli plugin install FILE`, `eudora-cli plugin list`, `eudora-cli plugin run ID COMMAND --ids 12,13`, `eudora-cli plugin remove ID`.
