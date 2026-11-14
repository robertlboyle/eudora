//! The natural voices' program (`eudora-voice`, downloaded on request: core `voices.rs`), run
//! while needed: started on the first thing to say, its speech passed to the window as events
//! ("voice": {id, part, wav} / {id, done} / {id, error}), stopped when Eudora closes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use tauri::Emitter;

struct Running {
    child: Child,
    stdin: ChildStdin,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// The speech being said (0: none): pieces of any other are dropped by the player.
static CURRENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the player is given: a piece of sound, or the end of a speech.
enum Play {
    Part { id: u64, part: u64, wav: Vec<u8> },
    Done { id: u64 },
}

/// Where Eudora plays the speech itself (Windows: `sounds::play_wav`), one piece after another on
/// a thread of its own, telling the window as each is played and when the speech ends.
fn player(handle: &tauri::AppHandle) -> &'static std::sync::Mutex<Sender<Play>> {
    static PLAYER: std::sync::OnceLock<std::sync::Mutex<Sender<Play>>> = std::sync::OnceLock::new();
    PLAYER.get_or_init(|| {
        let (tx, rx) = channel::<Play>();
        let handle = handle.clone();
        std::thread::Builder::new()
            .name("voice-player".into())
            .spawn(move || {
                use std::sync::atomic::Ordering;
                for item in rx {
                    match item {
                        Play::Part { id, part, wav } => {
                            if CURRENT.load(Ordering::SeqCst) != id {
                                continue;
                            }
                            if !eudora_core::sounds::play_wav(&wav) {
                                log(&format!("speech {id}: part {part} couldn't be played by Windows"));
                                let _ = handle.emit("voice", serde_json::json!({ "id": id, "error": "Windows couldn't play the natural voice's sound." }));
                                continue;
                            }
                            log(&format!("speech {id}: part {part} played"));
                            let _ = handle.emit("voice", serde_json::json!({ "id": id, "played": part }));
                        }
                        Play::Done { id } => {
                            let _ = handle.emit("voice", serde_json::json!({ "id": id, "done": true, "native": true }));
                        }
                    }
                }
            })
            .expect("a thread for the natural voice");
        std::sync::Mutex::new(tx)
    })
}

/// `voices/voice.log`: what the program was asked and answered (no audio, no text read), and
/// what it wrote to its error output, so a voice that stays silent says why.
fn log(line: &str) {
    let Some(dir) = eudora_core::voices::folder() else { return };
    let path = dir.join("voice.log");
    // (kept small: started again past 1 MB)
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {line}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}
/// Who waits for the list of voices (the next "voices" line goes to them).
static VOICES_WAITER: Mutex<Option<Sender<serde_json::Value>>> = Mutex::new(None);

/// The program running (started if needed, with the downloaded voices checked first).
fn with_program<T>(handle: &tauri::AppHandle, f: impl FnOnce(&mut Running) -> Result<T, String>) -> Result<T, String> {
    let mut slot = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = slot.as_mut() {
        if let Some(status) = r.child.try_wait().ok().flatten() {
            log(&format!("the program had ended ({status}); starting it again"));
            *slot = None; // (it ended: started again below)
        }
    }
    if slot.is_none() {
        let (_, dir) = eudora_core::voices::installed().ok_or("The natural voices aren't downloaded: Settings ▸ General ▸ Speech ▸ Download Natural Voices.")?;
        let program = dir.join(eudora_core::voices::program_name());
        let mut cmd = Command::new(&program);
        // its error output goes into the log
        let errors = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("voice.log")).map(Stdio::from).unwrap_or_else(|_| Stdio::null());
        cmd.arg("--models").arg(&dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(errors);
        log(&format!("starting {}", program.display()));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(|e| {
            log(&format!("couldn't start it: {e}"));
            format!("The natural voices couldn't start: {e}")
        })?;
        let stdin = child.stdin.take().ok_or("no input")?;
        let stdout = child.stdout.take().ok_or("no output")?;
        let handle = handle.clone();
        std::thread::Builder::new()
            .name("voice-reader".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                        log(&format!("an answer that isn't JSON: {}", line.chars().take(200).collect::<String>()));
                        continue;
                    };
                    match (value.get("id"), value.get("wav"), value.get("error"), value.get("done")) {
                        (Some(id), Some(wav), _, _) => log(&format!("speech {id}: part {} ({} KB of sound) passed to the window", value["part"], wav.as_str().map_or(0, |w| w.len() * 3 / 4 / 1024))),
                        (id, _, Some(e), _) => log(&format!("speech {}: error: {e}", id.map(ToString::to_string).unwrap_or_default())),
                        (Some(id), _, _, Some(_)) => log(&format!("speech {id}: done")),
                        _ if value.get("voices").is_some() => log("the list of voices"),
                        _ => {}
                    }
                    if value.get("voices").is_some() || (value.get("error").is_some() && value.get("id").is_none()) {
                        if let Some(tx) = VOICES_WAITER.lock().unwrap_or_else(|e| e.into_inner()).take() {
                            let _ = tx.send(value);
                            continue;
                        }
                    }
                    // Eudora plays the sound itself where it can, in order; the window only hears
                    // what's happening (the "native" parts carry no sound)
                    if eudora_core::sounds::plays_wav() {
                        let id = value["id"].as_u64().unwrap_or(0);
                        if let Some(wav) = value["wav"].as_str() {
                            use base64::Engine;
                            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(wav) {
                                let part = value["part"].as_u64().unwrap_or(0);
                                let _ = player(&handle).lock().unwrap_or_else(|e| e.into_inner()).send(Play::Part { id, part, wav: bytes });
                                let _ = handle.emit("voice", serde_json::json!({ "id": id, "part": part, "native": true }));
                            }
                            continue;
                        }
                        if value.get("done").is_some() && value.get("id").is_some() {
                            let _ = player(&handle).lock().unwrap_or_else(|e| e.into_inner()).send(Play::Done { id });
                            continue;
                        }
                    }
                    let _ = handle.emit("voice", value);
                }
                log("the program closed its output (it ended)");
            })
            .map_err(|e| e.to_string())?;
        *slot = Some(Running { child, stdin });
    }
    f(slot.as_mut().expect("started"))
}

fn send(r: &mut Running, value: serde_json::Value) -> Result<(), String> {
    writeln!(r.stdin, "{value}").and_then(|_| r.stdin.flush()).map_err(|e| format!("The natural voices stopped: {e}"))
}

/// Says `text` (speech comes back as "voice" events with this `id`).
pub fn say(handle: &tauri::AppHandle, id: u64, text: &str, voice: &str, speed: f32) -> Result<(), String> {
    log(&format!("speech {id}: asked to say {} characters with {voice} at speed {speed}", text.chars().count()));
    // (a new speech replaces the one being played)
    CURRENT.store(id, std::sync::atomic::Ordering::SeqCst);
    eudora_core::sounds::stop_wav();
    with_program(handle, |r| send(r, serde_json::json!({ "op": "say", "id": id, "text": text, "voice": voice, "speed": speed })))
}

/// A note from the window into the log (what it saw when a voice stayed silent).
pub fn note(text: &str) {
    log(&format!("the window: {text}"));
}

/// Stops what's being said (no-op when the program isn't running).
pub fn stop() {
    CURRENT.store(0, std::sync::atomic::Ordering::SeqCst);
    eudora_core::sounds::stop_wav();
    if let Some(r) = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = send(r, serde_json::json!({ "op": "stop" }));
    }
}

/// The voices the model has: [{id, name}].
pub fn voices(handle: &tauri::AppHandle) -> Result<serde_json::Value, String> {
    let (tx, rx) = channel();
    *VOICES_WAITER.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    with_program(handle, |r| send(r, serde_json::json!({ "op": "voices" })))?;
    // (the first request loads the model: a second or so)
    let answer = rx.recv_timeout(Duration::from_secs(60)).map_err(|_| "The natural voices didn't answer.".to_string())?;
    if let Some(e) = answer.get("error") {
        return Err(e.as_str().unwrap_or("The natural voices couldn't start.").to_string());
    }
    Ok(english_voices(&answer["voices"]))
}

/// The English voices only, each with its name, American then British, female then male (Heart,
/// the usual voice, first):. Kokoro's ids are an accent and a gender, then a name (af_ =
/// American female, bm_ = British male…); its other languages' voices (e, f, h, i, j, p, z) are
/// left out, because Eudora turns the text into English sounds first and they couldn't say it
/// right.
fn english_voices(list: &serde_json::Value) -> serde_json::Value {
    let mut out: Vec<(u8, String, String)> = Vec::new();
    for v in list.as_array().into_iter().flatten() {
        let Some(id) = v["id"].as_str() else { continue };
        let Some((code, name)) = id.split_once('_') else { continue };
        let (accent, rank_accent) = match code.chars().next() {
            Some('a') => ("US", 0),
            Some('b') => ("UK", 2),
            _ => continue,
        };
        let (gender, rank_gender) = match code.chars().nth(1) {
            Some('f') => ("female", 0),
            Some('m') => ("male", 1),
            _ => continue,
        };
        let mut shown: String = name.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
        shown.push_str(&name.chars().skip(1).collect::<String>());
        out.push((rank_accent + rank_gender, id.to_string(), format!("{shown} ({accent}, {gender})")));
    }
    out.sort_by(|a, b| (a.0, a.1 != "af_heart", &a.2).cmp(&(b.0, b.1 != "af_heart", &b.2)));
    serde_json::Value::Array(out.into_iter().map(|(_, id, name)| serde_json::json!({ "id": id, "name": name })).collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_english_voices_with_names_in_order() {
        let list = serde_json::json!([
            { "id": "af_alloy", "name": "af_alloy" }, { "id": "bm_lewis", "name": "bm_lewis" }, { "id": "af_heart", "name": "Heart" },
            { "id": "am_puck", "name": "am_puck" }, { "id": "jf_alpha", "name": "jf_alpha" }, { "id": "zm_yunxi", "name": "zm_yunxi" }, { "id": "bf_lily", "name": "bf_lily" },
        ]);
        let got: Vec<String> = super::english_voices(&list).as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(got, ["Heart (US, female)", "Alloy (US, female)", "Puck (US, male)", "Lily (UK, female)", "Lewis (UK, male)"]);
    }
}

/// Ends the program (Eudora closing, or the voices removed).
pub fn quit() {
    if let Some(mut r) = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = send(&mut r, serde_json::json!({ "op": "quit" }));
        std::thread::sleep(Duration::from_millis(100));
        let _ = r.child.kill();
    }
}
