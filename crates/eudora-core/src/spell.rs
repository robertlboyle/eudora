//! Edit ▸ Check Spelling: the system's own spell checker (Windows' Spell Checking API, the one
//! Word and Edge use, with the user's own dictionary; the Mac: NSSpellChecker, the one every Mac
//! program uses, with its languages and learned words). The window walks the misspellings it
//! finds with their suggestions.

use serde::Serialize;

use crate::error::Result;

/// One misspelled word in a piece of text: where (in UTF-16 units, as the window's text is
/// counted), the word, and what to put instead.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Misspelling {
    pub start: usize,
    pub len: usize,
    pub word: String,
    /// "replace": `suggestions[0]` is the one fix (a common typo); "delete": a repeated word;
    /// else suggestions to choose from.
    pub action: String,
    pub suggestions: Vec<String>,
}

/// Checks each piece of text (the window sends the text of each paragraph). The language is
/// the user's (Windows' display language, else English).
pub fn check(texts: &[String]) -> Result<Vec<Vec<Misspelling>>> {
    system::check(texts)
}

/// Adds a word to the user's dictionary (the system's: every program that uses it learns it).
pub fn add_word(word: &str) -> Result<()> {
    system::add(word)
}

/// Whether this computer can check spelling.
pub fn available() -> bool {
    system::checker().is_ok()
}

#[cfg(windows)]
mod system {
    use windows::Win32::Globalization::{
        CORRECTIVE_ACTION_DELETE, CORRECTIVE_ACTION_REPLACE, GetUserDefaultLocaleName, ISpellChecker, ISpellCheckerFactory, SpellCheckerFactory,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
    use windows::core::{HSTRING, PWSTR};

    use super::Misspelling;
    use crate::error::{Error, Result};

    fn err(e: windows::core::Error) -> Error {
        Error::Other(format!("the spell checker: {e}"))
    }

    pub fn checker() -> Result<ISpellChecker> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED); // (already done on this thread is fine)
            let factory: ISpellCheckerFactory = CoCreateInstance(&SpellCheckerFactory, None, CLSCTX_INPROC_SERVER).map_err(err)?;
            let mut buffer = [0u16; 85];
            let n = GetUserDefaultLocaleName(&mut buffer);
            let mine = String::from_utf16_lossy(&buffer[..(n.max(1) as usize - 1)]);
            for tag in [mine.as_str(), "en-US"] {
                if !tag.is_empty() && factory.IsSupported(&HSTRING::from(tag)).map_err(err)?.as_bool() {
                    return factory.CreateSpellChecker(&HSTRING::from(tag)).map_err(err);
                }
            }
            Err(Error::Other("this computer has no spell checker for its language (Windows Settings ▸ Time & language ▸ Language: add the language's basic typing)".into()))
        }
    }

    fn take(p: PWSTR) -> String {
        if p.is_null() {
            return String::new();
        }
        unsafe {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        }
    }

    pub fn check(texts: &[String]) -> Result<Vec<Vec<Misspelling>>> {
        let checker = checker()?;
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            if text.trim().is_empty() {
                out.push(Vec::new()); // (Windows refuses empty text)
                continue;
            }
            let units: Vec<u16> = text.encode_utf16().collect();
            let mut found = Vec::new();
            unsafe {
                let errors = checker.Check(&HSTRING::from(text.as_str())).map_err(err)?;
                loop {
                    let mut next = None;
                    let hr = errors.Next(&mut next);
                    let Some(e) = next.filter(|_| hr.is_ok()) else { break };
                    let (start, len) = (e.StartIndex().map_err(err)? as usize, e.Length().map_err(err)? as usize);
                    let word = String::from_utf16_lossy(units.get(start..start + len).unwrap_or_default());
                    let action = e.CorrectiveAction().map_err(err)?;
                    let (action, suggestions) = if action == CORRECTIVE_ACTION_REPLACE {
                        ("replace", vec![take(e.Replacement().map_err(err)?)])
                    } else if action == CORRECTIVE_ACTION_DELETE {
                        ("delete", Vec::new())
                    } else {
                        let mut list = Vec::new();
                        if let Ok(e) = checker.Suggest(&HSTRING::from(word.as_str())) {
                            let mut one = [PWSTR::null()];
                            while list.len() < 8 {
                                let mut got = 0u32;
                                if e.Next(&mut one, Some(&mut got)).is_err() || got == 0 {
                                    break;
                                }
                                list.push(take(one[0]));
                            }
                        }
                        ("suggest", list)
                    };
                    found.push(Misspelling { start, len, word, action: action.into(), suggestions });
                }
            }
            out.push(found);
        }
        Ok(out)
    }

    pub fn add(word: &str) -> Result<()> {
        unsafe { checker()?.Add(&HSTRING::from(word)).map_err(err) }
    }
}

#[cfg(target_os = "macos")]
mod system {
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSArray, NSRange, NSString};

    use super::Misspelling;
    use crate::error::Result;

    pub struct Ready;

    /// Every Mac has one (its languages: System Settings ▸ Keyboard ▸ Text Input).
    pub fn checker() -> Result<Ready> {
        Ok(Ready)
    }

    fn shared() -> Retained<AnyObject> {
        // SAFETY: a class method with no arguments, returning the shared checker
        unsafe { msg_send![class!(NSSpellChecker), sharedSpellChecker] }
    }

    /// (AppKit's checker is used from the main thread: the window calls from a worker thread,
    /// so it waits for the main thread; the command line is on it already)
    pub fn check(texts: &[String]) -> Result<Vec<Vec<Misspelling>>> {
        Ok(dispatch2::run_on_main(|_| check_here(texts)))
    }

    /// The same on this thread (the tests: their main thread never runs the main queue).
    pub fn check_here(texts: &[String]) -> Vec<Vec<Misspelling>> {
        autoreleasepool(|_| texts.iter().map(|t| check_one(t)).collect())
    }

    fn check_one(text: &str) -> Vec<Misspelling> {
        let checker = shared();
        let string = NSString::from_str(text);
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut found = Vec::new();
        let mut at = 0usize;
        while at < units.len() {
            // SAFETY: the documented selector with its argument types; no word count asked for
            let range: NSRange = unsafe {
                msg_send![&checker, checkSpellingOfString: &*string, startingAt: at as isize, language: None::<&NSString>, wrap: false, inSpellDocumentWithTag: 0isize, wordCount: std::ptr::null_mut::<isize>()]
            };
            if range.length == 0 || range.location < at || range.location >= units.len() {
                break; // (NSNotFound: nothing more)
            }
            let (start, len) = (range.location, range.length.min(units.len() - range.location));
            // SAFETY: the documented selector; the range lies inside the string
            let guesses: Option<Retained<NSArray<NSString>>> =
                unsafe { msg_send![&checker, guessesForWordRange: range, inString: &*string, language: None::<&NSString>, inSpellDocumentWithTag: 0isize] };
            let suggestions = guesses.map(|g| g.iter().take(8).map(|s| s.to_string()).collect()).unwrap_or_default();
            found.push(Misspelling { start, len, word: String::from_utf16_lossy(&units[start..start + len]), action: "suggest".into(), suggestions });
            at = start + len;
        }
        found
    }

    pub fn add(word: &str) -> Result<()> {
        let word = word.to_string();
        dispatch2::run_on_main(move |_| {
            // SAFETY: the documented selector, taking the word to learn
            unsafe {
                let _: () = msg_send![&shared(), learnWord: &*NSString::from_str(&word)];
            }
        });
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod system {
    use super::Misspelling;
    use crate::error::{Error, Result};

    pub struct Unavailable;

    pub fn checker() -> Result<Unavailable> {
        Err(Error::Other("spell checking on this system comes later".into()))
    }

    pub fn check(_texts: &[String]) -> Result<Vec<Vec<Misspelling>>> {
        checker().map(|_| Vec::new())
    }

    pub fn add(_word: &str) -> Result<()> {
        checker().map(|_| ())
    }
}

#[cfg(all(test, any(windows, target_os = "macos")))]
mod tests {
    #[test]
    fn misspellings_are_found_with_suggestions() {
        if !super::available() {
            return; // (a computer without its language's spell checker)
        }
        let texts = ["This sentense has a mistake.".to_string(), "All fine here.".to_string(), String::new()];
        #[cfg(windows)]
        let found = super::check(&texts).unwrap();
        #[cfg(target_os = "macos")]
        let found = super::system::check_here(&texts);
        assert_eq!(found[0].len(), 1, "{found:?}");
        let m = &found[0][0];
        assert_eq!((m.word.as_str(), m.start, m.len), ("sentense", 5, 8));
        assert!(m.suggestions.iter().any(|s| s == "sentence"), "{m:?}");
        assert!(found[1].is_empty());
    }
}
