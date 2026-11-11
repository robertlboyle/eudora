//! Signing in on iOS: Apple's sign-in sheet (ASWebAuthenticationSession). iOS freezes an app
//! whose page is in Safari, so the desktops' way (a page that comes back to `http://localhost`,
//! where Eudora listens) never gets its answer there. The sheet shows Microsoft's or Google's
//! page over Eudora and hands back the address the page ends on (`<scheme>://…?code=…`), which
//! is registered with Microsoft and Google for this app.
//!
//! Per-OS by nature: no cross-platform library does this; it's one call into AuthenticationServices.

use crate::{Error, Result};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, AnyProtocol, Bool, NSObject, NSObjectProtocol};
use objc2::{class, define_class, msg_send, ClassType, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSString, NSURL};
use std::cell::RefCell;
use std::sync::mpsc;
use std::time::Duration;

#[link(name = "AuthenticationServices", kind = "framework")]
unsafe extern "C" {}

/// How long the sheet may stay open before Eudora gives up (as the desktops' sign-in).
const WAIT: Duration = Duration::from_secs(300);

define_class!(
    // The window the sheet shows over (iOS asks for one: `presentationContextProvider`).
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EudoraSignInAnchor"]
    struct Anchor;

    impl Anchor {
        #[unsafe(method(presentationAnchorForWebAuthenticationSession:))]
        fn anchor(&self, _session: &AnyObject) -> *mut AnyObject {
            key_window()
        }
    }

    unsafe impl NSObjectProtocol for Anchor {}
);

thread_local! {
    // The sheet showing, kept alive until it ends (it holds its anchor only weakly). Main thread only.
    static SHOWING: RefCell<Option<(Retained<AnyObject>, Retained<Anchor>)>> = const { RefCell::new(None) };
}

/// Eudora's window: the key window of a connected scene, else the first window there.
pub(crate) fn key_window() -> *mut AnyObject {
    unsafe {
        let app: *mut AnyObject = msg_send![class!(UIApplication), sharedApplication];
        if app.is_null() {
            return std::ptr::null_mut();
        }
        let scenes: *mut AnyObject = msg_send![app, connectedScenes];
        let scenes: *mut AnyObject = if scenes.is_null() { scenes } else { msg_send![scenes, allObjects] };
        let count: usize = if scenes.is_null() { 0 } else { msg_send![scenes, count] };
        let mut first: *mut AnyObject = std::ptr::null_mut();
        for i in 0..count {
            let scene: *mut AnyObject = msg_send![scenes, objectAtIndex: i];
            let is_window_scene: Bool = msg_send![scene, respondsToSelector: objc2::sel!(windows)];
            if !is_window_scene.as_bool() {
                continue;
            }
            let windows: *mut AnyObject = msg_send![scene, windows];
            let n: usize = if windows.is_null() { 0 } else { msg_send![windows, count] };
            for j in 0..n {
                let window: *mut AnyObject = msg_send![windows, objectAtIndex: j];
                let key: Bool = msg_send![window, isKeyWindow];
                if key.as_bool() {
                    return window;
                }
                if first.is_null() {
                    first = window;
                }
            }
        }
        first
    }
}

/// Shows `url` in the sign-in sheet and waits for the page to end on an address starting
/// `<scheme>:`; that address. Canceled by the user = an error saying so.
pub fn authenticate(url: &str, scheme: &str) -> Result<String> {
    let (tx, rx) = mpsc::channel::<Result<String>>();
    let (url, scheme) = (url.to_string(), scheme.to_string());
    let started = dispatch2::run_on_main(move |mtm| start(mtm, &url, &scheme, tx));
    started?;
    match rx.recv_timeout(WAIT) {
        Ok(answer) => answer,
        Err(_) => {
            dispatch2::run_on_main(|_| {
                if let Some((session, _)) = SHOWING.with(|s| s.borrow_mut().take()) {
                    let _: () = unsafe { msg_send![&*session, cancel] };
                }
            });
            Err(Error::Other("the sign-in wasn't finished within 5 minutes".into()))
        }
    }
}

fn start(mtm: MainThreadMarker, url: &str, scheme: &str, tx: mpsc::Sender<Result<String>>) -> Result<()> {
    // (one at a time: a sheet left over from before is closed)
    if let Some((old, _)) = SHOWING.with(|s| s.borrow_mut().take()) {
        let _: () = unsafe { msg_send![&*old, cancel] };
    }
    let page = NSURL::URLWithString(&NSString::from_str(url)).ok_or_else(|| Error::Other(format!("not a web address: {url}")))?;
    let done = block2::RcBlock::new(move |callback: *mut NSURL, error: *mut AnyObject| {
        SHOWING.with(|s| s.borrow_mut().take());
        let answer = if let Some(callback) = unsafe { callback.as_ref() } {
            match callback.absoluteString() {
                Some(address) => Ok(address.to_string()),
                None => Err(Error::Other("the sign-in came back without an address".into())),
            }
        } else if error.is_null() {
            Err(Error::Other("the sign-in ended without an answer".into()))
        } else {
            // ASWebAuthenticationSessionErrorCodeCanceledLogin = 1
            let code: isize = unsafe { msg_send![error, code] };
            if code == 1 {
                Err(Error::Other("the sign-in was canceled".into()))
            } else {
                let why: Option<Retained<NSString>> = unsafe { msg_send![error, localizedDescription] };
                Err(Error::Other(format!("the sign-in sheet stopped: {}", why.map(|w| w.to_string()).unwrap_or_default())))
            }
        };
        let _ = tx.send(answer);
    });
    unsafe {
        let session: Allocated<AnyObject> = msg_send![class!(ASWebAuthenticationSession), alloc];
        let session: Option<Retained<AnyObject>> =
            msg_send![session, initWithURL: &*page, callbackURLScheme: &*NSString::from_str(scheme), completionHandler: &*done];
        let session = session.ok_or_else(|| Error::Other("iOS's sign-in sheet isn't available".into()))?;
        let anchor: Retained<Anchor> = msg_send![Anchor::alloc(mtm), init];
        if let Some(protocol) = AnyProtocol::get(c"ASWebAuthenticationPresentationContextProviding") {
            objc2::ffi::class_addProtocol(Anchor::class() as *const _ as *mut _, protocol);
        }
        let _: () = msg_send![&*session, setPresentationContextProvider: &*anchor];
        SHOWING.with(|s| *s.borrow_mut() = Some((session.clone(), anchor)));
        let ok: Bool = msg_send![&*session, start];
        if !ok.as_bool() {
            SHOWING.with(|s| s.borrow_mut().take());
            return Err(Error::Other("iOS didn't open the sign-in sheet".into()));
        }
    }
    Ok(())
}

/// The authorization code in the address the sheet came back with, checked against `state`.
pub fn code_in(address: &str, state: &str) -> Result<String> {
    let url = url::Url::parse(address).map_err(|e| Error::Other(format!("the sign-in came back to an address Eudora can't read: {e}")))?;
    let get = |k: &str| url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
    if let Some(e) = get("error") {
        return Err(Error::Other(format!("sign-in canceled or refused: {e} {}", get("error_description").unwrap_or_default())));
    }
    if get("state").as_deref() != Some(state) {
        return Err(Error::Other("the sign-in reply didn't match this request (state)".into()));
    }
    get("code").ok_or_else(|| Error::Other("the sign-in came back without a code".into()))
}
