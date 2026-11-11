//! Voice typing into a message. For now the system's own: on Windows, voice typing (Win+H) types
//! into whatever has the keyboard, so the message window's 🎤 button puts the caret in the text
//! and presses Win+H for the user. A dictation engine of Eudora's own (Whisper, through cpal +
//! whisper-rs: approved) comes later, for the Mac and Linux too.
//!
//! Per-OS code: Windows' SendInput, as nothing cross-platform presses the Windows key.

use crate::error::{Error, Result};

/// Starts the system's voice typing in the window in front.
#[cfg(windows)]
pub fn start() -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_LWIN};
    let key = |vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let h = VIRTUAL_KEY(b'H' as u16);
    let none = KEYBD_EVENT_FLAGS(0);
    let inputs = [key(VK_LWIN, none), key(h, none), key(h, KEYEVENTF_KEYUP), key(VK_LWIN, KEYEVENTF_KEYUP)];
    // SAFETY: a plain array of keyboard events of the size given.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(Error::Other("Windows didn't take the key press: press Windows+H to start voice typing".into()));
    }
    Ok(())
}

/// The Mac: AppKit's own Start Dictation (Edit ▸ Start Dictation), sent to the text that has the
/// keyboard. The Mac asks once whether to turn dictation on.
#[cfg(target_os = "macos")]
pub fn start() -> Result<()> {
    use objc2::runtime::AnyObject;
    use objc2::sel;
    let sent = dispatch2::run_on_main(|mtm| {
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        unsafe { app.sendAction_to_from(sel!(startDictation:), None, None::<&AnyObject>) }
    });
    if sent {
        Ok(())
    } else {
        Err(Error::Other("the Mac didn't start dictation: press the Microphone key (or Fn twice)".into()))
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn start() -> Result<()> {
    Err(Error::Other("this system has no voice typing Eudora can start yet".into()))
}
