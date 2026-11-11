//! What only Android needs from the app (the Android-only side of the phone work; the shared
//! phone screens are `ui/phone/`).
//!
//! **Certificates:** Eudora checks every server's certificate with rustls-platform-verifier,
//! which on Android asks Android's own trust store through Java. It must be given the app's Java
//! VM and context once, before the first connection, or every secure connection (Microsoft,
//! Google, IMAP, SMTP) fails. Its Java half comes from the rustls project's Maven repository
//! (`gen/android/build.gradle.kts`).
//!
//! **Checking for mail while the app is closed:** Android's WorkManager runs `CheckWorker`
//! (`gen/android/…/CheckWorker.kt`) about every 15 minutes when there's a network; it calls
//! `EudoraNative.checkOnce` here, which does one pass over every account (the core's
//! `watch::download_all`, as the open app's download does) and sends what's due in the Out
//! mailbox. While the app is open its own download holds the library, and the job lets it be.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use jni::objects::{Global, JByteArray, JClass, JObject, JString};
use jni::sys::jint;
use jni::{Env, JValue, jni_sig, jni_str};

/// The app's Java VM and context, for calls into Java from any thread: from Tauri's setup (the
/// open app) or from the background job's call (where Tauri isn't running).
static APP: OnceLock<(jni::JavaVM, Global<JObject<'static>>)> = OnceLock::new();

/// Keeps the VM and context, and sets up what needs them: the certificate check
/// (rustls-platform-verifier) and the secrets' sealing with the Keystore's key.
fn adopt(env: &mut Env, context: &JObject) -> Result<(), jni::errors::Error> {
    if APP.get().is_none() {
        let _ = APP.set((env.get_java_vm()?, env.new_global_ref(context)?));
    }
    let local = env.new_local_ref(context)?;
    rustls_platform_verifier::android::init_with_env(env, local)?;
    eudora_core::android_secrets::set_sealer(Box::new(|bytes, seal| secret_box(bytes, seal)));
    Ok(())
}

/// Runs `f` with a JNI environment and the app's context.
fn with_app<T>(f: impl FnOnce(&mut Env, &JObject) -> Result<T, jni::errors::Error>) -> Result<T, String> {
    let (vm, context) = APP.get().ok_or_else(|| "the app's Java context isn't known yet".to_string())?;
    vm.attach_current_thread(|env| f(env, context)).map_err(|e: jni::errors::Error| e.to_string())
}

/// Sets up the open app's Java side (from `ndk-context`, which Tauri sets up when the app
/// starts): certificates checked through Android, secrets sealed with the Keystore. Called once,
/// early in setup.
pub fn init_certificates() -> Result<(), String> {
    let ctx = ndk_context::android_context();
    // (the VM and the context are the app's own, valid for as long as it runs)
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) };
    vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
        // (a wrapper around the app's global reference, only to make references from; a JObject
        // frees nothing when dropped)
        let app = unsafe { JObject::from_raw(env, ctx.context().cast()) };
        adopt(env, &app)
    })
    .map_err(|e| format!("Android's certificate check couldn't be set up: {e}"))
}

/// One of the app's own classes, found through the app's class loader (a thread Rust started
/// only sees Android's own classes).
fn app_class<'local>(env: &mut Env<'local>, context: &JObject, name: &str) -> Result<JClass<'local>, jni::errors::Error> {
    let loader = env.call_method(context, jni_str!("getClassLoader"), jni_sig!("()Ljava/lang/ClassLoader;"), &[])?.l()?;
    let name = env.new_string(name)?;
    let class = env.call_method(&loader, jni_str!("loadClass"), jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"), &[JValue::Object(&name)])?.l()?;
    env.cast_local::<JClass>(class)
}

/// Seals (`seal`) or opens a secret with the Keystore's key: `SecretBox.kt`.
fn secret_box(bytes: &[u8], seal: bool) -> Result<Vec<u8>, String> {
    with_app(|env, context| {
        let class = app_class(env, context, "net.planet.eudora27.SecretBox")?;
        let input = env.byte_array_from_slice(bytes)?;
        let method = if seal { jni_str!("seal") } else { jni_str!("open") };
        let out = env.call_static_method(&class, method, jni_sig!("([B)[B"), &[JValue::Object(&input)]);
        if out.is_err() {
            env.exception_clear();
        }
        let out = env.cast_local::<JByteArray>(out?.l()?)?;
        env.convert_byte_array(&out)
    })
}

/// A file picked in Android's file picker, open for reading.
pub struct Picked {
    pub name: String,
    pub file: std::fs::File,
    /// (when the place it's in says)
    pub size: Option<u64>,
}

/// A file descriptor Android handed over, as a file of ours (closed when it's dropped).
fn owned_file(fd: i32) -> Option<std::fs::File> {
    use std::os::fd::FromRawFd;
    // (detached from its ParcelFileDescriptor in DocumentPicker.kt: nothing else closes it)
    (fd >= 0).then(|| unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Shows Android's file picker for backups (several files for a backup in parts) and waits for
/// the person: none when canceled. Called on a thread that may wait (never the main one).
pub fn pick_backups() -> Result<Vec<Picked>, String> {
    let answer = with_app(|env, context| -> Result<Option<String>, jni::errors::Error> {
        let class = app_class(env, context, "net.planet.eudora27.DocumentPicker")?;
        let out = env.call_static_method(&class, jni_str!("openBackups"), jni_sig!("()Ljava/lang/String;"), &[]);
        if out.is_err() {
            env.exception_clear();
        }
        let out = out?.l()?;
        if out.is_null() {
            return Ok(None);
        }
        Ok(Some(env.cast_local::<JString>(out)?.try_to_string(env)?))
    })?;
    let Some(answer) = answer else { return Ok(Vec::new()) };
    let fields: Vec<&str> = answer.split('\0').collect();
    Ok(fields
        .chunks(3)
        .filter_map(|f| match f {
            [name, fd, size] => Some(Picked { name: name.to_string(), file: owned_file(fd.parse().ok()?)?, size: size.parse().ok().filter(|&n: &u64| n > 0) }),
            _ => None,
        })
        .collect())
}

/// Shows Android's picker for a new file (`name` suggested; the person picks where) and waits:
/// the file, open for writing, or none when canceled.
pub fn save_as(name: &str) -> Result<Option<std::fs::File>, String> {
    let fd = with_app(|env, context| -> Result<jint, jni::errors::Error> {
        let class = app_class(env, context, "net.planet.eudora27.DocumentPicker")?;
        let name = env.new_string(name)?;
        let out = env.call_static_method(&class, jni_str!("saveAs"), jni_sig!("(Ljava/lang/String;)I"), &[JValue::Object(&name)]);
        if out.is_err() {
            env.exception_clear();
        }
        out?.i()
    })?;
    Ok(owned_file(fd))
}

/// The file `save_as` gave has been written and closed (or abandoned): Android is told, so the
/// place it's in lists it whole (without this, Downloads showed a saved backup as 0 bytes).
pub fn saved() {
    let _ = with_app(|env, context| -> Result<(), jni::errors::Error> {
        let class = app_class(env, context, "net.planet.eudora27.DocumentPicker")?;
        if env.call_static_method(&class, jni_str!("saved"), jni_sig!("()V"), &[]).is_err() {
            env.exception_clear();
        }
        Ok(())
    });
}

/// While you sign in in the browser (`on`: the sign-in page opened; off: the sign-in ended),
/// `SignInService` keeps Eudora from being frozen in the background, so the sign-in page's answer
/// (sent back to http://localhost on the phone) reaches it at once: tried on Android 16 without
/// it, the answer arrived 11 minutes late. Errors are only said in the log: signing in still
/// works, just slowly.
pub fn keep_answering(on: bool) {
    let done = with_app(|env, app| -> Result<(), jni::errors::Error> {
        // an Intent naming the service (Android's own classes are found from any thread)
        let package = env.call_method(&app, jni_str!("getPackageName"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
        let service = env.new_string("net.planet.eudora27.SignInService")?;
        let intent_class = env.find_class(jni_str!("android/content/Intent"))?;
        let intent = env.new_object(&intent_class, jni_sig!("()V"), &[])?;
        env.call_method(
            &intent,
            jni_str!("setClassName"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;"),
            &[JValue::Object(&package), JValue::Object(&service)],
        )?;
        if !on {
            env.call_method(&app, jni_str!("stopService"), jni_sig!("(Landroid/content/Intent;)Z"), &[JValue::Object(&intent)])?;
            return Ok(());
        }
        // (startForegroundService is Android 8 and newer; before it, startService)
        let started = env.call_method(&app, jni_str!("startForegroundService"), jni_sig!("(Landroid/content/Intent;)Landroid/content/ComponentName;"), &[JValue::Object(&intent)]);
        if started.is_err() {
            env.exception_clear();
            env.call_method(&app, jni_str!("startService"), jni_sig!("(Landroid/content/Intent;)Landroid/content/ComponentName;"), &[JValue::Object(&intent)])?;
        }
        Ok(())
    });
    if let Err(e) = done {
        eprintln!("the sign-in service didn't {} ({e}): signing in still works, but Android may hold its answer back", if on { "start" } else { "stop" });
    }
}

/// The file, in Android's data folder for the app, that names Eudora's own folder (Tauri's
/// `app_data_dir`), so the background job finds the same library as the app.
const WHERE_FILE: &str = "eudora-app-data-dir.txt";

/// Called in setup: remembers where the app keeps its library, for the background job.
pub fn remember_data_dir(android_data_dir: &Path, app_data_dir: &Path) {
    let _ = std::fs::write(android_data_dir.join(WHERE_FILE), app_data_dir.to_string_lossy().as_bytes());
}

/// What the background job hears: events go to the download log, as the app's download writes.
struct JobLog(std::sync::Mutex<Option<std::fs::File>>);

impl eudora_core::Progress for JobLog {
    fn report(&self, _done: u64, _total: u64, _stage: &str) {}
    fn event(&self, text: &str) {
        use std::io::Write;
        if let Some(f) = self.0.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{}  (background) {text}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
        }
    }
}

/// One background check: 0 = done (or nothing to do), 1 = the app is open and downloading
/// itself, 2 = it failed (WorkManager tries again later).
fn check_once(android_data_dir: &Path) -> jint {
    let Ok(text) = std::fs::read_to_string(android_data_dir.join(WHERE_FILE)) else { return 0 }; // (the app hasn't run yet)
    eudora_core::library::set_app_data_dir(PathBuf::from(text.trim()));
    let Some(root) = eudora_core::library::default_library_path() else { return 0 };
    if !root.join("eudora.db").is_file() {
        return 0;
    }
    let log = JobLog(std::sync::Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(root.join("logs").join("download.log")).ok()));
    let mut library = match eudora_core::Library::open(&root, eudora_core::Access::Write, &log) {
        Ok(l) => l,
        Err(eudora_core::Error::Locked(_)) => return 1,
        Err(e) => {
            eudora_core::Progress::event(&log, &format!("The background check couldn't open the library ({e})."));
            return 2;
        }
    };
    // no account yet (a new install): nothing to check, and nothing wrong
    let no_accounts = library.accounts_counted(false).map(|a| a.is_empty()).unwrap_or(false)
        && eudora_core::graph::GraphClient::signed_in_accounts().is_empty()
        && eudora_core::google::signed_in_accounts().is_empty();
    // how much mail to keep not chosen yet (the phone asks): nothing downloaded until it is, as
    // the open app's download waits too
    if no_accounts || eudora_core::graph_sync::download_period(&library).is_none() {
        let _ = library.close(&eudora_core::Silent);
        return 0;
    }
    // (the archive is left to the open app: a background job has about ten minutes)
    let w =eudora_core::watch::Watch { every: None, ..eudora_core::watch::Watch::background(false) };
    let result = eudora_core::watch::sign_ins(&library, &w, &log).and_then(|mut clients| eudora_core::watch::download_all(&mut library, &mut clients, &w.options, &log));
    let code = match result {
        Ok(reports) => {
            let new: u64 = reports.iter().map(|r| r.counts.imported).sum();
            if new > 0 {
                eudora_core::Progress::event(&log, &format!("{new} new messages"));
            }
            if let Ok(sent) = eudora_core::outbox::send_due(&library, false) {
                sent.notes.iter().for_each(|n| eudora_core::Progress::event(&log, n));
            }
            // older mail let go to stay within the period or space chosen (a fresh process each
            // time, so no memory of the last trim: it's quick when nothing is over)
            eudora_core::watch::trim_when_due(&library, &mut None, &log);
            0
        }
        Err(e) => {
            eudora_core::Progress::event(&log, &format!("The background check didn't finish ({e}); WorkManager tries again later."));
            2
        }
    };
    let _ = library.close(&eudora_core::Silent);
    code
}

/// `EudoraNative.checkOnce(context, dataDir)` (Kotlin): the background check, called by
/// `CheckWorker` with the app's context and Android's data folder for it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_net_planet_eudora27_EudoraNative_checkOnce<'caller>(
    mut unowned_env: jni::EnvUnowned<'caller>,
    _class: JClass<'caller>,
    context: JObject<'caller>,
    data_dir: JString<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jint> {
                // (certificates checked through Android, secrets sealed with the Keystore, here too;
            // set up once per process)
            adopt(env, &context)?;
            let dir = data_dir.try_to_string(env)?;
            Ok(check_once(Path::new(&dir)))
        })
        .resolve::<jni::errors::LogErrorAndDefault>()
}
