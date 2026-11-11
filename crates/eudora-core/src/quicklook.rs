//! Opening an attachment on iOS: Apple's Quick Look viewer (QLPreviewController) over Eudora,
//! which shows PDFs, pictures, Office files and the like, and has the system's Share button
//! (save to Files, send to another app). iOS has no "open with the default program", so this is
//! the phone's Open.
//!
//! Per-OS by nature: one call into QuickLook, as `web_auth.rs` is into AuthenticationServices.

use crate::{Error, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, AnyProtocol, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSString, NSURL};
use std::cell::RefCell;
use std::path::Path;

#[link(name = "QuickLook", kind = "framework")]
unsafe extern "C" {}

pub struct Ivars {
    file: Retained<NSURL>,
}

define_class!(
    // The viewer's one file (its data source).
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EudoraQuickLookFile"]
    #[ivars = Ivars]
    struct Source;

    impl Source {
        #[unsafe(method(numberOfPreviewItemsInPreviewController:))]
        fn count(&self, _controller: &AnyObject) -> isize {
            1
        }

        #[unsafe(method(previewController:previewItemAtIndex:))]
        fn item(&self, _controller: &AnyObject, _index: isize) -> *mut AnyObject {
            Retained::as_ptr(&self.ivars().file) as *mut AnyObject
        }
    }

    unsafe impl NSObjectProtocol for Source {}
);

thread_local! {
    // The viewer's data source, kept while it shows (the viewer holds it weakly). Main thread only.
    static SHOWING: RefCell<Option<Retained<Source>>> = const { RefCell::new(None) };
}

/// Shows the file in Quick Look over Eudora's window.
pub fn preview(path: &Path) -> Result<()> {
    let path = path.to_string_lossy().into_owned();
    dispatch2::run_on_main(move |mtm| show(mtm, &path))
}

/// The system's share sheet for the file: Save to Files, open it in another app, AirDrop, send
/// it on.
pub fn share(path: &Path) -> Result<()> {
    let path = path.to_string_lossy().into_owned();
    dispatch2::run_on_main(move |_| unsafe {
        let file = NSURL::fileURLWithPath(&NSString::from_str(&path));
        let items = objc2_foundation::NSArray::from_retained_slice(&[Retained::into_super(Retained::into_super(file))]);
        let top = top_controller()?;
        let class = objc2::runtime::AnyClass::get(c"UIActivityViewController").ok_or_else(|| Error::Other("iOS's share sheet isn't available".into()))?;
        let sheet: Retained<AnyObject> =
            msg_send![msg_send![class, alloc], initWithActivityItems: &*items, applicationActivities: std::ptr::null_mut::<AnyObject>()];
        // (an iPad shows it as a popover, which needs a place to point from)
        let popover: *mut AnyObject = msg_send![&*sheet, popoverPresentationController];
        if !popover.is_null() {
            let view: *mut AnyObject = msg_send![top, view];
            let _: () = msg_send![popover, setSourceView: view];
        }
        let _: () = msg_send![top, presentViewController: &*sheet, animated: true, completion: std::ptr::null_mut::<AnyObject>()];
        Ok(())
    })
}

/// The view controller showing on top (over any sheet already presented).
unsafe fn top_controller() -> Result<*mut AnyObject> {
    let window = crate::web_auth::key_window();
    let mut top: *mut AnyObject = if window.is_null() { std::ptr::null_mut() } else { unsafe { msg_send![window, rootViewController] } };
    if top.is_null() {
        return Err(Error::Other("Eudora's window isn't ready to show the attachment".into()));
    }
    loop {
        let above: *mut AnyObject = unsafe { msg_send![top, presentedViewController] };
        if above.is_null() {
            return Ok(top);
        }
        top = above;
    }
}

fn show(mtm: MainThreadMarker, path: &str) -> Result<()> {
    let file = NSURL::fileURLWithPath(&NSString::from_str(path));
    let source = Source::alloc(mtm).set_ivars(Ivars { file });
    let source: Retained<Source> = unsafe { msg_send![super(source), init] };
    unsafe {
        if let Some(protocol) = AnyProtocol::get(c"QLPreviewControllerDataSource") {
            objc2::ffi::class_addProtocol(Source::class() as *const _ as *mut _, protocol);
        }
        // over whatever is showing already (a sheet, the sign-in sheet…)
        let top = top_controller()?;
        let class = objc2::runtime::AnyClass::get(c"QLPreviewController").ok_or_else(|| Error::Other("Quick Look isn't available in this copy of Eudora".into()))?;
        let viewer: Retained<AnyObject> = msg_send![msg_send![class, alloc], init];
        let _: () = msg_send![&*viewer, setDataSource: &*source];
        SHOWING.with(|s| *s.borrow_mut() = Some(source));
        let _: () = msg_send![top, presentViewController: &*viewer, animated: true, completion: std::ptr::null_mut::<AnyObject>()];
    }
    Ok(())
}
