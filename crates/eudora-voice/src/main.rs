//! `eudora-voice`: Eudora's natural voices, a program of its own that Eudora downloads only when
//! asked. Eudora starts it with `--models <folder>` and talks to it by lines of JSON:
//!
//! in:  {"op":"voices"}                                   → {"voices":[{"id","name"}…]}
//!      {"op":"say","id":7,"text":"…","voice":"af_heart","speed":1.0}
//!                                                         → {"id":7,"part":0,"wav":"<base64>"} …
//!                                                           {"id":7,"done":true}
//!      {"op":"stop"}           (the speech under way stops after the piece being made)
//!      {"op":"quit"}
//! out: {"error":"…"} (and "id" when it was about a say)
//!
//! Speech comes a sentence at a time, so Eudora starts playing the first while the rest are made.
//! Nothing goes online; the text never leaves the computer. Also: `eudora-voice --models DIR
//! --say "text" --out file.wav` (for trying it), `--version`, `--notices` (the licenses of what
//! it is built from).

mod kokoro;
mod nrl_rules;
mod words;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use base64::Engine;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    text: String,
    #[serde(default)]
    voice: String,
    #[serde(default)]
    speed: Option<f32>,
}

/// When the program last had something to do (seconds since 1970).
static LAST: AtomicU64 = AtomicU64::new(0);
const IDLE_SECS: u64 = 600;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn touch() {
    LAST.store(now(), Ordering::SeqCst);
}

fn say_line(out: &Mutex<std::io::Stdout>, value: serde_json::Value) {
    touch();
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if args.iter().any(|a| a == "--notices") {
        print!("{}", include_str!("../NOTICES.txt"));
        return;
    }
    if args.iter().any(|a| a == "--version") {
        println!("eudora-voice {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let models = arg("--models").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));

    // a one-off, for trying it: --say "text" --out file.wav [--voice id] [--speed 1.0]
    if let Some(text) = arg("--say") {
        let started = std::time::Instant::now();
        let engine = match kokoro::Kokoro::load(&models) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        let loaded = started.elapsed();
        let voice = arg("--voice").unwrap_or_else(|| "af_heart".into());
        let speed = arg("--speed").and_then(|s| s.parse().ok()).unwrap_or(1.0);
        let mut all = Vec::new();
        for piece in kokoro::pieces(&text) {
            println!("{piece}\n  {}", engine.phonemes(&piece, &voice));
            match engine.speak(&piece, &voice, speed) {
                Ok(s) => all.extend(s),
                Err(e) => eprintln!("{e}"),
            }
        }
        let secs = all.len() as f32 / kokoro::SAMPLE_RATE as f32;
        let took = started.elapsed() - loaded;
        println!("loaded in {loaded:.2?}; {secs:.1} s of speech in {took:.2?} ({:.1}x real time)", secs / took.as_secs_f32().max(0.001));
        // (what the model gave: silence, or NaN, shows here)
        let nan = all.iter().filter(|s| s.is_nan()).count();
        let peak = all.iter().filter(|s| s.is_finite()).fold(0.0f32, |m, s| m.max(s.abs()));
        println!("samples: {}, peak {peak:.4}, not a number: {nan}", all.len());
        if let Some(out) = arg("--out") {
            let _ = std::fs::write(out, kokoro::wav(&all));
        }
        return;
    }

    let out = Arc::new(Mutex::new(std::io::stdout()));
    touch();
    // the speech under way (a stop, or a newer say, makes it stop between pieces)
    let current = Arc::new(AtomicU64::new(0));
    // (the model holds about 300 MB: after 10 quiet minutes the program ends, and Eudora starts it
    // again when there is something to say)
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        if now() - LAST.load(Ordering::SeqCst) > IDLE_SECS {
            std::process::exit(0);
        }
    });
    let (tx, rx) = mpsc::channel::<Request>();

    // the maker: one thread, the model loaded on first use
    let maker = {
        let out = out.clone();
        let current = current.clone();
        std::thread::spawn(move || {
            let mut engine: Option<kokoro::Kokoro> = None;
            let load = |engine: &mut Option<kokoro::Kokoro>| -> Result<(), String> {
                if engine.is_none() {
                    *engine = Some(kokoro::Kokoro::load(&models)?);
                }
                Ok(())
            };
            for req in rx {
                touch();
                match req.op.as_str() {
                    "voices" => match load(&mut engine) {
                        Ok(()) => {
                            let list: Vec<_> = engine.as_ref().map(|e| e.voices()).unwrap_or_default().into_iter().map(|(id, name)| json!({ "id": id, "name": name })).collect();
                            say_line(&out, json!({ "voices": list }));
                        }
                        Err(e) => say_line(&out, json!({ "error": e })),
                    },
                    "say" => {
                        if let Err(e) = load(&mut engine) {
                            say_line(&out, json!({ "id": req.id, "error": e }));
                            continue;
                        }
                        let Some(engine) = engine.as_ref() else { continue };
                        let voice = if req.voice.is_empty() { "af_heart" } else { req.voice.as_str() };
                        for (part, piece) in kokoro::pieces(&req.text).iter().enumerate() {
                            if current.load(Ordering::SeqCst) != req.id {
                                break;
                            }
                            match engine.speak(piece, voice, req.speed.unwrap_or(1.0)) {
                                Ok(samples) if !samples.is_empty() => {
                                    let wav = base64::engine::general_purpose::STANDARD.encode(kokoro::wav(&samples));
                                    say_line(&out, json!({ "id": req.id, "part": part, "wav": wav }));
                                }
                                Ok(_) => {}
                                Err(e) => say_line(&out, json!({ "id": req.id, "error": e })),
                            }
                        }
                        say_line(&out, json!({ "id": req.id, "done": true }));
                        touch();
                    }
                    _ => {}
                }
            }
        })
    };

    // the reader: requests as they come (a stop acts at once, not after the speech in hand)
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        touch();
        let req: Request = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                say_line(&out, json!({ "error": format!("not a request: {e}") }));
                continue;
            }
        };
        match req.op.as_str() {
            "quit" => break,
            "stop" => current.store(0, Ordering::SeqCst),
            "say" => {
                current.store(req.id, Ordering::SeqCst);
                let _ = tx.send(req);
            }
            _ => {
                let _ = tx.send(req);
            }
        }
    }
    drop(tx);
    let _ = maker.join();
}
