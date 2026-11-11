//! The system's own icon for a kind of file (an attachment's chip shows the icon Explorer /
//! Finder shows for it). Windows: the shell's icon for the extension (SHGetFileInfo with
//! SHGFI_USEFILEATTRIBUTES: the file needn't exist, nothing is written), as a PNG. The Mac:
//! NSWorkspace's icon for the extension (Finder's), drawn at 32 points. On Linux there's none,
//! and the window keeps its own paperclip.

use std::collections::HashMap;
use std::sync::Mutex;

/// The extension of a file name, lowercase, letters and digits only (at most 16): "" when none.
pub fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 16 && ext.chars().all(|c| c.is_ascii_alphanumeric()) => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// The system's icon for files named like `name` (32 × 32 pixels), as PNG bytes; kept for the
/// program's life, one per extension.
pub fn icon_png(name: &str) -> Option<Vec<u8>> {
    static CACHE: Mutex<Option<HashMap<String, Option<Vec<u8>>>>> = Mutex::new(None);
    let ext = extension(name);
    if let Some(found) = CACHE.lock().ok()?.get_or_insert_with(HashMap::new).get(&ext) {
        return found.clone();
    }
    let made = system::icon_png(&ext);
    CACHE.lock().ok()?.get_or_insert_with(HashMap::new).insert(ext, made.clone());
    made
}

#[cfg(windows)]
mod system {
    use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetObjectW, HBITMAP, HDC};
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW};
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};
    use windows::core::HSTRING;

    pub fn icon_png(ext: &str) -> Option<Vec<u8>> {
        // the shell asks for COM on the calling thread (already there is fine)
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let name = HSTRING::from(if ext.is_empty() { "file".to_string() } else { format!("file.{ext}") });
        let mut info = SHFILEINFOW::default();
        let found = unsafe { SHGetFileInfoW(&name, FILE_ATTRIBUTE_NORMAL, Some(&mut info), size_of::<SHFILEINFOW>() as u32, SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES) };
        if found == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let mut parts = ICONINFO::default();
        let picture = unsafe { GetIconInfo(info.hIcon, &mut parts) }.ok().and_then(|_| {
            let dc = unsafe { CreateCompatibleDC(None) };
            let made = rgba(dc, parts.hbmColor, parts.hbmMask);
            unsafe {
                let _ = DeleteDC(dc);
                let _ = DeleteObject(parts.hbmColor.into());
                let _ = DeleteObject(parts.hbmMask.into());
            }
            made
        });
        unsafe {
            let _ = DestroyIcon(info.hIcon);
        }
        let (w, h, pixels) = picture?;
        let image = image::RgbaImage::from_raw(w, h, pixels)?;
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).ok()?;
        Some(png.into_inner())
    }

    /// The icon's pixels as RGBA, top row first: its own alpha, else (old icons) from the mask.
    fn rgba(dc: HDC, color: HBITMAP, mask: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
        if color.is_invalid() {
            return None;
        }
        let mut bm = BITMAP::default();
        if unsafe { GetObjectW(color.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) } == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth.max(0) as u32, bm.bmHeight.max(0) as u32);
        if w == 0 || h == 0 || w > 256 || h > 256 {
            return None;
        }
        let read = |bitmap: HBITMAP| -> Option<Vec<u8>> {
            let mut header = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w as i32, biHeight: -(h as i32), biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let lines = unsafe { GetDIBits(dc, bitmap, 0, h, Some(buf.as_mut_ptr().cast()), &mut header, DIB_RGB_COLORS) };
            (lines == h as i32).then_some(buf)
        };
        let mut pixels = read(color)?;
        if pixels.chunks_exact(4).all(|p| p[3] == 0) {
            // no alpha: the mask's white pixels are see-through
            let mask = if mask.is_invalid() { None } else { read(mask) };
            for (i, p) in pixels.chunks_exact_mut(4).enumerate() {
                p[3] = match &mask {
                    Some(m) if m[i * 4] != 0 => 0,
                    _ => 255,
                };
            }
        }
        for p in pixels.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA → RGBA
        }
        Some((w, h, pixels))
    }
}

#[cfg(target_os = "macos")]
mod system {
    use objc2::encode::{Encoding, RefEncode};
    use objc2::rc::{Allocated, Retained, autoreleasepool};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSData, NSDictionary, NSPoint, NSRect, NSSize, NSString};

    /// Core Graphics' image (only passed between AppKit calls here).
    #[repr(C)]
    struct CGImage {
        _private: [u8; 0],
    }

    // SAFETY: CGImageRef is a pointer to the opaque struct CGImage
    unsafe impl RefEncode for CGImage {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGImage", &[]));
    }

    /// NSBitmapImageFileTypePNG
    const PNG: usize = 4;

    /// (AppKit's images are made on the main thread: the window asks from a worker thread)
    pub fn icon_png(ext: &str) -> Option<Vec<u8>> {
        let ext = ext.to_string();
        dispatch2::run_on_main(move |_| autoreleasepool(|_| draw(&ext)))
    }

    fn draw(ext: &str) -> Option<Vec<u8>> {
        // SAFETY: documented AppKit selectors with their argument types; every object checked
        unsafe {
            let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
            #[allow(deprecated)] // (its replacement takes a UTType: a framework more for an extension)
            let image: Option<Retained<AnyObject>> = msg_send![&workspace, iconForFileType: &*NSString::from_str(ext)];
            let image = image?;
            // the picture drawn for 32 × 32 points (Retina: 64 × 64 pixels)
            let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(32.0, 32.0));
            let cg: *mut CGImage = msg_send![&image, CGImageForProposedRect: &mut rect, context: std::ptr::null_mut::<AnyObject>(), hints: std::ptr::null_mut::<AnyObject>()];
            if cg.is_null() {
                return None;
            }
            let rep: Allocated<AnyObject> = msg_send![class!(NSBitmapImageRep), alloc];
            let rep: Option<Retained<AnyObject>> = msg_send![rep, initWithCGImage: cg];
            let none = NSDictionary::<NSString, AnyObject>::new();
            let png: Option<Retained<NSData>> = msg_send![&rep?, representationUsingType: PNG, properties: &*none];
            png.map(|d| d.to_vec())
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod system {
    pub fn icon_png(_ext: &str) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_read_safely() {
        assert_eq!(extension("Report.PDF"), "pdf");
        assert_eq!(extension("archive.tar.gz"), "gz");
        assert_eq!(extension("README"), "");
        assert_eq!(extension("odd.p d f"), "");
    }

    // (the Mac's can't be a test: AppKit draws it on the main thread, which a test's never
    // serves; `cargo run -p eudora-core --example mac_check` checks it there)
    #[cfg(windows)]
    #[test]
    fn the_system_gives_a_picture_for_a_pdf() {
        let png = icon_png("a.pdf").expect("an icon");
        assert_eq!(&png[1..4], b"PNG");
        assert!(icon_png("b.pdf").is_some()); // from the cache
    }
}
