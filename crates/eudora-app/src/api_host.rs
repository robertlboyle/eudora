//! The local API, served by the window while it's open (Settings ▸ Programs and plugins:
//! on by default while open; served without the window only when the user allows it, by
//! `eudora-cli api serve`). Every call goes through the window's own library connection and
//! `Library::api_call` (permissions, rate limits, the log).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::Manager;

use crate::App;

/// What the window does for API calls: lets go of its download while a job needs the library
/// to itself, and runs its background download.
pub struct WindowHost(pub tauri::AppHandle);

impl eudora_core::api::Host for WindowHost {
    fn exclusive(&self, work: &mut dyn FnMut() -> eudora_core::Result<serde_json::Value>) -> eudora_core::Result<serde_json::Value> {
        let bg = self.0.state::<crate::background::Background>();
        bg.hold(true);
        let result = work();
        bg.hold(false);
        result
    }

    fn background(&self, set: Option<&serde_json::Value>) -> eudora_core::Result<serde_json::Value> {
        use crate::background::{Options, SETTING};
        let app = self.0.state::<App>();
        let bg = self.0.state::<crate::background::Background>();
        let text = |e: String| eudora_core::Error::Other(e);
        if let Some(options) = set {
            let options: Options = serde_json::from_value(options.clone())?;
            let json = serde_json::to_string(&options)?;
            app.with(|l| l.set_setting(SETTING, &json)).map_err(text)?;
            bg.options_changed();
        }
        let options = Options::read(app.with(|l| l.setting(SETTING)).ok().flatten());
        let state = bg.state.lock().map(|s| s.clone()).map_err(|e| text(e.to_string()))?;
        Ok(serde_json::json!({ "state": state, "options": options }))
    }
}

pub struct ApiHost {
    stop: Mutex<Option<Arc<AtomicBool>>>,
    /// What it's doing, for Settings.
    pub state: Arc<Mutex<String>>,
}

impl ApiHost {
    pub fn new() -> ApiHost {
        ApiHost { stop: Mutex::new(None), state: Arc::new(Mutex::new("Not started.".into())) }
    }

    /// Starts serving (if the user's setting allows it and it isn't already).
    pub fn start(&self, handle: tauri::AppHandle) {
        let app = handle.state::<App>();
        let Some(root) = app.path.clone() else { return };
        let on = app.with(|l| l.api_settings()).map(|s| s.with_window).unwrap_or(true);
        let mut stop = self.stop.lock().unwrap();
        if !on {
            *self.state.lock().unwrap() = "Off (Settings ▸ Programs and plugins).".into();
            return;
        }
        if stop.is_some() {
            return;
        }
        let flag = Arc::new(AtomicBool::new(false));
        *stop = Some(flag.clone());
        let limits = Arc::new(eudora_core::api::Limits::default());
        let caller = handle.clone();
        let handler: eudora_core::api_server::Handler = Arc::new(move |token, method, params| {
            let app = caller.state::<App>();
            let host = WindowHost(caller.clone());
            app.with(|l| Ok(l.api_call(&limits, &host, token, method, params)))
                .map_err(|e| eudora_core::api::Refusal { code: -32000, message: e })?
        });
        let state = self.state.clone();
        let address = eudora_core::api_server::address(&root);
        *state.lock().unwrap() = format!("Serving programs on {address} (this computer, your Windows account only).");
        std::thread::spawn(move || {
            if let Err(e) = eudora_core::api_server::serve(&root, handler, flag) {
                *state.lock().unwrap() = format!("Not serving: {e}");
            }
        });
    }

    /// Stops serving (the setting turned off, or Eudora closing).
    pub fn stop(&self, handle: &tauri::AppHandle) {
        if let Some(flag) = self.stop.lock().unwrap().take() {
            flag.store(true, Ordering::SeqCst);
            if let Some(root) = handle.state::<App>().path.clone() {
                eudora_core::api_server::wake(&root);
            }
            *self.state.lock().unwrap() = "Off.".into();
        }
    }
}
