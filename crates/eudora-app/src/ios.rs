//! iOS: new mail while Eudora is closed, its notifications, and the number on the app's icon
//!
//! iOS gives an app no timer of its own in the background: it wakes it now and then for a
//! "background app refresh" (`BGTaskScheduler`, about every 15 minutes at best, as iOS judges),
//! each time for about 30 seconds. Eudora's download is a thread of the app (`background.rs`),
//! so a wake-up only asks it to check now (Check Mail) and waits for the pass, then says what
//! came in. A notification per check (not per message): "3 new messages" with the newest's
//! sender and subject, for mail that arrived in an Inbox, unread, since the last one shown. The
//! icon's number is the Inbox's unread count. The user is asked once whether Eudora may notify.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;
use objc2_foundation::{NSDate, NSString};

#[link(name = "BackgroundTasks", kind = "framework")]
unsafe extern "C" {}
#[link(name = "UserNotifications", kind = "framework")]
unsafe extern "C" {}

/// The refresh task's name (also in Info.plist's BGTaskSchedulerPermittedIdentifiers).
const TASK: &str = "net.planet.eudora.check";
/// How long a wake-up waits for the download's pass (iOS allows about 30 s).
const WAIT: Duration = Duration::from_secs(25);
/// When the last notification's mail had come (unix seconds), beside the library.
const NOTIFIED: &str = "status/notified-until";

/// An Objective-C object moved to another thread (the task and its calls are thread-safe).
struct Sendable(Retained<AnyObject>);
unsafe impl Send for Sendable {}

/// At start, before the app finishes launching (iOS refuses a later registration): the
/// background check, and the first one asked for.
pub fn register() {
    let Some(class) = AnyClass::get(c"BGTaskScheduler") else { return };
    let launch = block2::RcBlock::new(|task: *mut AnyObject| {
        let Some(task) = (unsafe { Retained::retain(task) }) else { return };
        let task = Sendable(task);
        std::thread::spawn(move || {
            let task = task;
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stopping = stop.clone();
            let expired = block2::RcBlock::new(move || stopping.store(true, std::sync::atomic::Ordering::Relaxed));
            let _: () = unsafe { msg_send![&*task.0, setExpirationHandler: &*expired] };
            let done = check_now(&stop);
            schedule();
            let _: () = unsafe { msg_send![&*task.0, setTaskCompleted: Bool::new(done)] };
        });
    });
    unsafe {
        let scheduler: *mut AnyObject = msg_send![class, sharedScheduler];
        let _: Bool = msg_send![scheduler, registerForTaskWithIdentifier: &*NSString::from_str(TASK), usingQueue: std::ptr::null_mut::<AnyObject>(), launchHandler: &*launch];
    }
    schedule();
}

/// Asks iOS for the next wake-up, in about 15 minutes (it decides when).
pub fn schedule() {
    let Some(class) = AnyClass::get(c"BGAppRefreshTaskRequest") else { return };
    unsafe {
        let request: Retained<AnyObject> = msg_send![msg_send![class, alloc], initWithIdentifier: &*NSString::from_str(TASK)];
        let when = NSDate::dateWithTimeIntervalSinceNow(15.0 * 60.0);
        let _: () = msg_send![&*request, setEarliestBeginDate: &*when];
        let scheduler: *mut AnyObject = msg_send![AnyClass::get(c"BGTaskScheduler").unwrap(), sharedScheduler];
        let _: Bool = msg_send![scheduler, submitTaskRequest: &*request, error: std::ptr::null_mut::<*mut AnyObject>()];
    }
}

/// The library the app opened (its usual place on the phone).
fn library() -> Option<PathBuf> {
    eudora_core::library::usual_library().filter(|p| p.join("eudora.db").exists())
}

/// One wake-up: Check Mail, wait for the pass (or iOS's end), then notify and set the number.
fn check_now(stop: &std::sync::atomic::AtomicBool) -> bool {
    let Some(root) = library() else { return true };
    let asked = eudora_core::library::now();
    if eudora_core::graph_sync::request_check(&root).is_err() {
        return false;
    }
    let started = Instant::now();
    while started.elapsed() < WAIT && !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        // the request taken and a pass reported since: done
        let taken = !root.join(eudora_core::graph_sync::CHECK_FILE).exists();
        let passed = eudora_core::graph_sync::read_status(&root).is_some_and(|s| s.updated > asked && s.state != "running");
        if taken && passed {
            break;
        }
    }
    notify_new(&root);
    true
}

/// While the app runs (iOS pauses this thread with the app): every 30 s the icon's number is
/// kept right and new mail announced when Eudora isn't on screen (`after_pass`).
pub fn keep_watch() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(30));
        if let Some(root) = library() {
            after_pass(&root);
        }
    });
}

/// While Eudora is on screen the new mail is simply seen (the number on the icon kept right);
/// in the background, it's announced.
pub fn after_pass(root: &Path) {
    // (UIApplicationStateActive = 0; asked on the main thread)
    let active = dispatch2::run_on_main(|_| unsafe {
        let app: *mut AnyObject = msg_send![AnyClass::get(c"UIApplication").unwrap(), sharedApplication];
        let state: isize = msg_send![app, applicationState];
        state == 0
    });
    if active {
        seen(root);
    } else {
        notify_new(root);
    }
}

/// While Eudora is on screen: nothing is announced, the icon's number kept right.
fn seen(root: &Path) {
    let _ = std::fs::write(root.join(NOTIFIED), eudora_core::library::now().to_string());
    notify_new(root);
}

/// Mail that came into an Inbox, unread, since the last notification: one notification, and the
/// icon's number.
pub fn notify_new(root: &Path) {
    let Ok(conn) = rusqlite::Connection::open_with_flags(root.join("eudora.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return };
    let inbox = "f.path = 'Inbox' OR f.role = 'inbox'";
    let unread: i64 = conn
        .query_row(&format!("SELECT COALESCE(SUM(c.unread), 0) FROM folder_counts c JOIN folders f ON f.id = c.folder WHERE {inbox}"), [], |r| r.get(0))
        .unwrap_or(0);
    set_badge(unread);
    let marker = root.join(NOTIFIED);
    let since: i64 = std::fs::read_to_string(&marker).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0);
    if since == 0 {
        // (the first time: nothing old is announced)
        let _ = std::fs::write(&marker, eudora_core::library::now().to_string());
        return;
    }
    let new: Vec<(i64, String, String)> = conn
        .prepare(&format!(
            "SELECT m.added, m.from_display, m.subject FROM messages m JOIN locations l ON l.message = m.id JOIN folders f ON f.id = l.folder
             WHERE ({inbox}) AND m.added > ?1 AND m.flags & 1 = 1 ORDER BY m.added DESC LIMIT 50"
        ))
        .and_then(|mut s| s.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
        .unwrap_or_default();
    let Some((newest, from, subject)) = new.first().cloned() else { return };
    let _ = std::fs::write(&marker, newest.to_string());
    let title = if new.len() == 1 { from.clone() } else { format!("{} new messages", new.len()) };
    let body = if new.len() == 1 { subject } else { format!("{from}: {subject}") };
    post(&title, &body);
}

/// Asks once whether Eudora may notify and show a number on its icon.
pub fn ask_permission() {
    let Some(center) = center() else { return };
    let answered = block2::RcBlock::new(|_granted: Bool, _error: *mut AnyObject| {});
    // alert | badge | sound
    let options: usize = (1 << 2) | (1 << 0) | (1 << 1);
    let _: () = unsafe { msg_send![center, requestAuthorizationWithOptions: options, completionHandler: &*answered] };
}

fn center() -> Option<*mut AnyObject> {
    let class = AnyClass::get(c"UNUserNotificationCenter")?;
    let center: *mut AnyObject = unsafe { msg_send![class, currentNotificationCenter] };
    (!center.is_null()).then_some(center)
}

fn post(title: &str, body: &str) {
    let (Some(center), Some(content), Some(request)) =
        (center(), AnyClass::get(c"UNMutableNotificationContent"), AnyClass::get(c"UNNotificationRequest"))
    else {
        return;
    };
    unsafe {
        let content: Retained<AnyObject> = msg_send![msg_send![content, alloc], init];
        let _: () = msg_send![&*content, setTitle: &*NSString::from_str(title)];
        let _: () = msg_send![&*content, setBody: &*NSString::from_str(body)];
        let sound: *mut AnyObject = msg_send![AnyClass::get(c"UNNotificationSound").unwrap(), defaultSound];
        let _: () = msg_send![&*content, setSound: sound];
        let id = NSString::from_str(&format!("new-mail-{}", eudora_core::library::now()));
        let request: *mut AnyObject = msg_send![request, requestWithIdentifier: &*id, content: &*content, trigger: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![center, addNotificationRequest: request, withCompletionHandler: std::ptr::null_mut::<AnyObject>()];
    }
}

fn set_badge(count: i64) {
    let Some(center) = center() else { return };
    let _: () = unsafe { msg_send![center, setBadgeCount: count.max(0) as isize, withCompletionHandler: std::ptr::null_mut::<AnyObject>()] };
}
