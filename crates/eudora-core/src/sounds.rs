//! The system's own sounds.

/// Plays the system's new-mail sound (Windows: "New Mail Notification" in Sound settings, the
/// sound Outlook and Thunderbird play). False where there's none to ask for (the Mac, Linux, or
/// Windows set to no sound): the window plays a bell of Eudora's own then.
pub fn play_new_mail() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
        // (SND_NODEFAULT: nothing, not the default beep, when no sound is set for it)
        unsafe { PlaySoundW(windows::core::w!("MailBeep"), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT).as_bool() }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Plays a WAV held in memory and returns when it has finished (or was stopped by `stop_wav`): the
/// natural voices' speech, played by Eudora itself rather than by its window, whose sound goes out
/// through a WebView process of its own that Windows can mute or send elsewhere. False where
/// Eudora can't play it itself (the Mac and Linux for now: the window plays it there).
pub fn play_wav(wav: &[u8]) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};
        if wav.len() < 44 || &wav[..4] != b"RIFF" {
            return false;
        }
        // (SND_MEMORY: the "name" is the sound itself; SND_SYNC: back when it ends)
        unsafe { PlaySoundW(windows::core::PCWSTR(wav.as_ptr().cast()), None, SND_MEMORY | SND_SYNC | SND_NODEFAULT).as_bool() }
    }
    #[cfg(not(windows))]
    {
        let _ = wav;
        false
    }
}

/// Whether `play_wav` plays sounds itself on this system.
pub fn plays_wav() -> bool {
    cfg!(windows)
}

/// Stops a sound `play_wav` is playing (from any thread).
pub fn stop_wav() {
    #[cfg(windows)]
    {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_PURGE};
        unsafe {
            let _ = PlaySoundW(windows::core::PCWSTR::null(), None, SND_PURGE);
        }
    }
}
