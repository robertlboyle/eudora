//! Reading the text in pictures (OCR), for what a receipt or a notice shows only as an image.
//! Windows: its built-in OCR (Windows.Media.Ocr; it decodes PNG, JPEG, GIF, BMP and TIFF
//! itself). The Mac: Apple's Vision (VNRecognizeTextRequest, the text recognition Photos and
//! Live Text use; it decodes pictures itself). Elsewhere, or when the system can't: `ocrs`
//! (see `fallback`).
//!
//! Pictures too small to hold text worth reading (logos, spacers, tracking pixels) are skipped.

/// Pictures smaller than this (bytes) aren't read: logos and spacers.
pub const MIN_PICTURE: usize = 6 * 1024;
/// Nor bigger than this.
pub const MAX_PICTURE: usize = 15 << 20;

/// The text in a picture (its bytes as in the message), when OCR can read it.
pub fn picture_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() < MIN_PICTURE || bytes.len() > MAX_PICTURE {
        return None;
    }
    // the system's own OCR, else ocrs (where the system has none)
    let text = if system_available() { system::read(bytes) } else { crate::ocr_fallback::read(bytes) };
    text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// The fallback alone (`eudora ocr --fallback`, to check it where the system has OCR).
pub fn fallback_text(bytes: &[u8]) -> Option<String> {
    crate::ocr_fallback::read(bytes).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// Whether this system has OCR of its own (else the fallback is used). Asked once.
pub fn system_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(system::available)
}

#[cfg(windows)]
mod system {
    use windows::Graphics::Imaging::{BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode, ExifOrientationMode, SoftwareBitmap, BitmapAlphaMode};
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

    /// Windows Runtime on this thread (each thread once; one already set up is fine).
    fn init() {
        thread_local! {
            static READY: () = {
                // SAFETY: initialising the Windows Runtime for this thread has no preconditions
                let _ = unsafe { windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED) };
            };
        }
        READY.with(|_| {});
    }

    pub fn available() -> bool {
        init();
        OcrEngine::TryCreateFromUserProfileLanguages().is_ok()
    }

    pub fn read(bytes: &[u8]) -> Option<String> {
        init();
        let run = || -> windows::core::Result<String> {
            let engine = OcrEngine::TryCreateFromUserProfileLanguages()?;
            let stream = InMemoryRandomAccessStream::new()?;
            let writer = DataWriter::CreateDataWriter(&stream)?;
            writer.WriteBytes(bytes)?;
            writer.StoreAsync()?.join()?;
            writer.DetachStream()?;
            stream.Seek(0)?;
            let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
            // the OCR engine reads pictures up to its largest size: bigger ones are scaled down
            let max = OcrEngine::MaxImageDimension()? as f64;
            let (w, h) = (decoder.PixelWidth()? as f64, decoder.PixelHeight()? as f64);
            let transform = BitmapTransform::new()?;
            if w > max || h > max {
                let scale = (max / w).min(max / h);
                transform.SetScaledWidth((w * scale) as u32)?;
                transform.SetScaledHeight((h * scale) as u32)?;
            }
            let bitmap: SoftwareBitmap = decoder
                .GetSoftwareBitmapTransformedAsync(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Premultiplied, &transform, ExifOrientationMode::RespectExifOrientation, ColorManagementMode::DoNotColorManage)?
                .join()?;
            let result = engine.RecognizeAsync(&bitmap)?.join()?;
            Ok(result.Text()?.to_string())
        };
        run().ok()
    }
}

#[cfg(target_os = "macos")]
mod system {
    use objc2::rc::{Allocated, Retained, autoreleasepool};
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{class, msg_send, sel};
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSRect, NSString};

    // Vision isn't linked by anything else
    #[link(name = "Vision", kind = "framework")]
    unsafe extern "C" {}

    /// VNRequestTextRecognitionLevelAccurate
    const ACCURATE: isize = 0;

    pub fn available() -> bool {
        AnyClass::get(c"VNRecognizeTextRequest").is_some()
    }

    /// (Vision works on any thread: the download reads pictures on its own)
    pub fn read(bytes: &[u8]) -> Option<String> {
        autoreleasepool(|_| recognize(bytes))
    }

    fn recognize(bytes: &[u8]) -> Option<String> {
        // SAFETY: Vision's documented selectors with their argument types; every object checked
        unsafe {
            let data = NSData::with_bytes(bytes);
            let options = NSDictionary::<NSString, AnyObject>::new();
            let handler: Allocated<AnyObject> = msg_send![class!(VNImageRequestHandler), alloc];
            let handler: Option<Retained<AnyObject>> = msg_send![handler, initWithData: &*data, options: &*options];
            let handler = handler?;
            let request: Retained<AnyObject> = msg_send![class!(VNRecognizeTextRequest), new];
            let _: () = msg_send![&request, setRecognitionLevel: ACCURATE];
            let _: () = msg_send![&request, setUsesLanguageCorrection: true];
            let detects: bool = msg_send![&request, respondsToSelector: sel!(setAutomaticallyDetectsLanguage:)];
            if detects {
                let _: () = msg_send![&request, setAutomaticallyDetectsLanguage: true]; // (macOS 13 and later)
            }
            let requests = NSArray::from_retained_slice(&[request.clone()]);
            let done: bool = msg_send![&handler, performRequests: &*requests, error: std::ptr::null_mut::<*mut AnyObject>()];
            if !done {
                return None; // (not a picture Vision can read)
            }
            let results: Option<Retained<NSArray<AnyObject>>> = msg_send![&request, results];
            // each line where it is on the picture: top to bottom (Vision counts from the
            // bottom), then left to right
            let mut lines: Vec<(f64, f64, String)> = Vec::new();
            for line in results?.iter() {
                let top: Option<Retained<NSArray<AnyObject>>> = msg_send![&line, topCandidates: 1usize];
                let Some(best) = top.and_then(|t| t.firstObject()) else { continue };
                let text: Option<Retained<NSString>> = msg_send![&best, string];
                let box_: NSRect = msg_send![&line, boundingBox];
                if let Some(text) = text {
                    lines.push((-(box_.origin.y + box_.size.height), box_.origin.x, text.to_string()));
                }
            }
            lines.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
            Some(lines.into_iter().map(|l| l.2).collect::<Vec<_>>().join("\n"))
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod system {
    // Others: none (the fallback reads instead).
    pub fn available() -> bool {
        false
    }

    pub fn read(_bytes: &[u8]) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_pictures_are_skipped_and_damaged_ones_give_nothing() {
        assert!(picture_text(&[0u8; 100]).is_none(), "a spacer");
        assert!(picture_text(&vec![7u8; MIN_PICTURE + 10]).is_none(), "not a picture");
    }
}
