//! OCR where the system has none (Linux, a Windows without OCR languages, the Mac until Vision
//! is written): `ocrs`, a Rust OCR engine (code MIT / Apache-2.0). Its two models (text
//! detection and recognition, about 10 MB) are **CC-BY-SA 4.0** (trained on HierText): accepted
//! as the fallback when the system has no OCR (2026-10-14); they're downloaded on first use, never shipped, and credited in the notices.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Where the models come from (the ocrs project's own).
const MODELS: [(&str, &str); 2] = [
    ("text-detection.onnx", "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.onnx"),
    ("text-recognition.onnx", "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.onnx"),
];

/// Where they're kept on this computer (outside the library: they're the same everywhere).
fn models_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| if cfg!(target_os = "macos") { PathBuf::from(h).join("Library").join("Caches") } else { PathBuf::from(h).join(".cache") }))?;
    Some(base.join("Eudora").join("ocr-models"))
}

/// The model files, downloaded the first time (None: no network, nowhere to keep them).
fn model_files() -> Option<(PathBuf, PathBuf)> {
    let dir = models_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let mut paths = Vec::new();
    for (name, url) in MODELS {
        let path = dir.join(name);
        if !path.is_file() {
            let mut response = ureq::get(url).call().ok()?;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut response.body_mut().with_config().limit(200 << 20).reader(), &mut bytes).ok()?;
            let part = path.with_extension("part");
            std::fs::write(&part, &bytes).ok()?;
            std::fs::rename(&part, &path).ok()?;
        }
        paths.push(path);
    }
    Some((paths[0].clone(), paths[1].clone()))
}

/// The engine, made once (None: the models couldn't be had).
fn engine() -> Option<&'static ocrs::OcrEngine> {
    static ENGINE: OnceLock<Option<ocrs::OcrEngine>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let (detection, recognition) = model_files()?;
            let detection = rten::Model::load_file(detection).ok()?;
            let recognition = rten::Model::load_file(recognition).ok()?;
            ocrs::OcrEngine::new(ocrs::OcrEngineParams { detection_model: Some(detection), recognition_model: Some(recognition), ..Default::default() }).ok()
        })
        .as_ref()
}

/// The text in a picture.
pub fn read(bytes: &[u8]) -> Option<String> {
    let engine = engine()?;
    let picture = image::load_from_memory(bytes).ok()?.into_rgb8();
    let source = ocrs::ImageSource::from_bytes(picture.as_raw(), picture.dimensions()).ok()?;
    let input = engine.prepare_input(source).ok()?;
    engine.get_text(&input).ok()
}
