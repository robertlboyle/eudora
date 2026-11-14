//! Speech through the `tts` library, the same on every system. The other choice, the system's own
//! speech, is the window's (the web view's speechSynthesis, over the system's voices).
//!
//! The library is kept to this file (swapping it later touches nothing else) and runs on one
//! thread of its own: requests go to it, so a slow voice never holds up the caller.

use serde::Serialize;

/// A voice to speak with.
#[derive(Clone, Debug, Serialize)]
pub struct Voice {
    pub id: String,
    pub name: String,
    /// "en-US"…
    pub language: String,
}

/// How to speak: a voice (None = the system's usual one), a speed (1.0 = usual, 0.5 to 2.0; 0 =
/// usual), and a volume (0.0 to 1.0; 0 = as it is).
#[derive(Clone, Debug, Default)]
pub struct Saying {
    pub voice: Option<String>,
    pub speed: f32,
    pub volume: f32,
}

#[cfg(not(target_os = "android"))]
mod imp {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::{Sender, channel};
    use std::time::Duration;

    enum Job {
        Speak(String, Saying, bool),
        Stop,
        Voices(Sender<Result<Vec<Voice>, String>>),
        Speaking(Sender<bool>),
    }

    static WORKER: Mutex<Option<Sender<Job>>> = Mutex::new(None);

    fn open() -> Result<tts::Tts, tts::Error> {
        // (Apple's systems: AVFoundation directly; the library's own choice on the Mac reads the
        // system's version by splitting text and would stop at a version with no minor number)
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        return tts::Tts::new(tts::Backends::AvFoundation);
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        tts::Tts::default()
    }

    /// The engine's rate for a speed (1.0 = its usual, 0.5 = half way to its slowest, 2.0 = its
    /// fastest).
    fn rate_for(speed: f32, normal: f32, min: f32, max: f32) -> f32 {
        let speed = if speed > 0.0 { speed.clamp(0.5, 2.0) } else { 1.0 };
        let rate = if speed >= 1.0 { normal + (max - normal) * (speed - 1.0) } else { normal - (normal - min) * (1.0 - speed) };
        rate.clamp(min.min(max), max.max(min))
    }

    fn worker() -> Result<Sender<Job>, String> {
        let mut slot = WORKER.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = slot.as_ref() {
            return Ok(tx.clone());
        }
        let (tx, rx) = channel::<Job>();
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("speech".into())
            .spawn(move || {
                let mut engine = match open() {
                    Ok(t) => {
                        let _ = ready_tx.send(Ok(()));
                        t
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("this computer's speech couldn't start: {e}")));
                        return;
                    }
                };
                let (normal, min, max) = (engine.normal_rate(), engine.min_rate(), engine.max_rate());
                let mut voice_now: Option<String> = None;
                for job in rx {
                    match job {
                        Job::Speak(text, how, interrupt) => {
                            if how.voice != voice_now {
                                if let Some(id) = &how.voice {
                                    if let Ok(list) = engine.voices() {
                                        if let Some(v) = list.iter().find(|v| &v.id() == id) {
                                            let _ = engine.set_voice(v);
                                        }
                                    }
                                }
                                voice_now = how.voice.clone();
                            }
                            let _ = engine.set_rate(rate_for(how.speed, normal, min, max));
                            if how.volume > 0.0 {
                                let (vmin, vmax) = (engine.min_volume(), engine.max_volume());
                                let _ = engine.set_volume(vmin + (vmax - vmin) * how.volume.clamp(0.0, 1.0));
                            }
                            let _ = engine.speak(text, interrupt);
                        }
                        Job::Stop => {
                            let _ = engine.stop();
                        }
                        Job::Voices(reply) => {
                            let list = engine
                                .voices()
                                .map(|vs| vs.iter().map(|v| Voice { id: v.id(), name: v.name(), language: v.language().to_string() }).collect())
                                .map_err(|e| e.to_string());
                            let _ = reply.send(list);
                        }
                        Job::Speaking(reply) => {
                            let _ = reply.send(engine.is_speaking().unwrap_or(false));
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv_timeout(Duration::from_secs(15)).map_err(|_| "this computer's speech didn't start".to_string())??;
        *slot = Some(tx.clone());
        Ok(tx)
    }

    pub fn speak(text: &str, how: &Saying, interrupt: bool) -> Result<(), String> {
        worker()?.send(Job::Speak(text.to_string(), how.clone(), interrupt)).map_err(|_| "speech stopped".to_string())
    }

    pub fn stop() -> Result<(), String> {
        worker()?.send(Job::Stop).map_err(|_| "speech stopped".to_string())
    }

    pub fn voices() -> Result<Vec<Voice>, String> {
        let (tx, rx) = channel();
        worker()?.send(Job::Voices(tx)).map_err(|_| "speech stopped".to_string())?;
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "the voices didn't answer".to_string())?
    }

    pub fn speaking() -> bool {
        let (tx, rx) = channel();
        worker().ok().and_then(|w| w.send(Job::Speaking(tx)).ok()).and_then(|_| rx.recv_timeout(Duration::from_secs(2)).ok()).unwrap_or(false)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn speeds_stay_within_the_engines_range() {
            // (WinRT's range: 0.5 to 6, usual 1)
            assert_eq!(super::rate_for(1.0, 1.0, 0.5, 6.0), 1.0);
            assert_eq!(super::rate_for(0.0, 1.0, 0.5, 6.0), 1.0);
            assert_eq!(super::rate_for(2.0, 1.0, 0.5, 6.0), 6.0);
            assert_eq!(super::rate_for(0.5, 1.0, 0.5, 6.0), 0.75);
            assert_eq!(super::rate_for(9.0, 1.0, 0.5, 6.0), 6.0);
        }
    }
}

#[cfg(target_os = "android")]
mod imp {
    use super::*;
    const NOT_HERE: &str = "Eudora's speech library isn't on Android yet: choose the system's speech";
    pub fn speak(_: &str, _: &Saying, _: bool) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    pub fn stop() -> Result<(), String> {
        Ok(())
    }
    pub fn voices() -> Result<Vec<Voice>, String> {
        Err(NOT_HERE.into())
    }
    pub fn speaking() -> bool {
        false
    }
}

/// Says `text` (`interrupt`: stops what's being said first).
pub fn speak(text: &str, how: &Saying, interrupt: bool) -> Result<(), String> {
    imp::speak(text, how, interrupt)
}

/// Stops speaking.
pub fn stop() -> Result<(), String> {
    imp::stop()
}

/// The voices this computer has.
pub fn voices() -> Result<Vec<Voice>, String> {
    imp::voices()
}

/// Whether it's speaking now.
pub fn speaking() -> bool {
    imp::speaking()
}
