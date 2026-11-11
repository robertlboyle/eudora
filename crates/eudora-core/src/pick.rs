//! The system's own Open dialog, to choose files or a folder (File ▸ Open File, Import…, and
//! Browse… beside every path the window asks for). Windows: the shell's IFileOpenDialog,
//! through the `windows` crate already used; the Mac: AppKit's NSOpenPanel, through the `objc2`
//! bindings Tauri already builds there (2026-10-27; it must be shown from the main thread, so
//! the window calls `pick` there).

use std::path::PathBuf;

use crate::error::Result;

/// What to choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    File,
    Files,
    Folder,
}

/// Shows the dialog (owned by the window `owner`, a native window handle, when given) and
/// returns what was chosen: nothing when canceled. `filters`: (name, "*.mbx;*.mbox") pairs.
pub fn pick(owner: Option<isize>, what: Pick, title: &str, filters: &[(&str, &str)]) -> Result<Vec<PathBuf>> {
    let (title, filters): (String, Vec<(String, String)>) = (title.to_string(), filters.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect());
    // Windows: a thread of its own, as the dialog needs a single-threaded COM apartment; the Mac:
    // right here, on the main thread (AppKit shows panels only from there)
    if cfg!(windows) {
        std::thread::spawn(move || system::pick(owner, what, &title, &filters))
            .join()
            .map_err(|_| crate::error::Error::Other("the Open dialog stopped unexpectedly".into()))?
    } else {
        system::pick(owner, what, &title, &filters)
    }
}

#[cfg(windows)]
mod system {
    use std::path::PathBuf;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
    use windows::core::{HSTRING, PCWSTR};

    use super::Pick;
    use crate::error::{Error, Result};

    pub fn pick(owner: Option<isize>, what: Pick, title: &str, filters: &[(String, String)]) -> Result<Vec<PathBuf>> {
        let err = |e: windows::core::Error| Error::Other(format!("the Open dialog: {e}"));
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let result = (|| -> Result<Vec<PathBuf>> {
                let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(err)?;
                let mut options = dialog.GetOptions().map_err(err)? | FOS_FORCEFILESYSTEM;
                match what {
                    Pick::Folder => options |= FOS_PICKFOLDERS,
                    Pick::Files => options |= FOS_ALLOWMULTISELECT,
                    Pick::File => {}
                }
                dialog.SetOptions(options).map_err(err)?;
                dialog.SetTitle(&HSTRING::from(title)).map_err(err)?;
                // (the strings must live while the dialog does)
                let names: Vec<HSTRING> = filters.iter().map(|(n, _)| HSTRING::from(n.as_str())).collect();
                let specs: Vec<HSTRING> = filters.iter().map(|(_, s)| HSTRING::from(s.as_str())).collect();
                if what != Pick::Folder && !filters.is_empty() {
                    let list: Vec<COMDLG_FILTERSPEC> =
                        names.iter().zip(&specs).map(|(n, s)| COMDLG_FILTERSPEC { pszName: PCWSTR(n.as_ptr()), pszSpec: PCWSTR(s.as_ptr()) }).collect();
                    dialog.SetFileTypes(&list).map_err(err)?;
                }
                if dialog.Show(owner.map(|h| HWND(h as *mut _))).is_err() {
                    return Ok(Vec::new()); // canceled
                }
                let items = dialog.GetResults().map_err(err)?;
                let mut out = Vec::new();
                for i in 0..items.GetCount().map_err(err)? {
                    let item = items.GetItemAt(i).map_err(err)?;
                    let name = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(err)?;
                    out.push(PathBuf::from(name.to_string().unwrap_or_default()));
                    CoTaskMemFree(Some(name.0 as *const _));
                }
                Ok(out)
            })();
            CoUninitialize();
            result
        }
    }
}

#[cfg(target_os = "macos")]
mod system {
    use std::path::PathBuf;

    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};
    use objc2_foundation::{NSArray, NSString};

    use super::Pick;
    use crate::error::{Error, Result};

    pub fn pick(_owner: Option<isize>, what: Pick, title: &str, filters: &[(String, String)]) -> Result<Vec<PathBuf>> {
        let mtm = MainThreadMarker::new().ok_or_else(|| Error::Other("the Open dialog can only be shown from the main thread".into()))?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(what != Pick::Folder);
        panel.setCanChooseDirectories(what == Pick::Folder);
        panel.setCanCreateDirectories(what == Pick::Folder);
        panel.setAllowsMultipleSelection(what == Pick::Files);
        panel.setMessage(Some(&NSString::from_str(title)));
        // The Mac's panel has no list of filters to switch between: it's limited to the filters'
        // kinds only when none of them is "every file", so it never refuses a file the Windows
        // dialog would let through.
        let any = filters.iter().any(|(_, spec)| spec.split(';').any(|p| matches!(p.trim(), "*" | "*.*")));
        let kinds: Vec<_> = filters.iter().flat_map(|(_, spec)| spec.split(';')).filter_map(|p| p.trim().strip_prefix("*.")).map(NSString::from_str).collect();
        if what != Pick::Folder && !any && !kinds.is_empty() {
            #[allow(deprecated)] // (its replacement takes UTType, a framework more for a list of extensions)
            panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&kinds)));
        }
        if panel.runModal() != NSModalResponseOK {
            return Ok(Vec::new()); // canceled
        }
        Ok(panel.URLs().iter().filter_map(|url| url.path()).map(|p| PathBuf::from(p.to_string())).collect())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod system {
    use std::path::PathBuf;

    use super::Pick;
    use crate::error::{Error, Result};

    pub fn pick(_owner: Option<isize>, _what: Pick, _title: &str, _filters: &[(String, String)]) -> Result<Vec<PathBuf>> {
        Err(Error::Other("choosing files this way on this system comes later: type the path".into()))
    }
}
