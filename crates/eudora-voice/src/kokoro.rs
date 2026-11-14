//! Kokoro (82 million parameters, Apache-2.0) run on rten, pure Rust: text → misaki's phonemes →
//! Kokoro's token ids → 24 kHz speech. The model and voices are the files PlanetPDF downloads
//! (`kokoro-v1.0.onnx`, `voices-v1.0.bin`, from thewh1teagle/kokoro-onnx's model-files-v1.0).

use std::collections::HashMap;
use std::path::Path;

use rten_tensor::prelude::*;
use rten_tensor::{NdTensor, Tensor};

pub const SAMPLE_RATE: u32 = 24_000;
/// The vocoder's STFT of its source signal (its last axis: real, imaginary), whose exact zeros
/// `Kokoro::speak` nudges before the phase is taken.
const STFT_NODE: &str = "/decoder/decoder/generator/STFT_output_0";

/// Kokoro takes at most 510 tokens at once (512 with the two ends).
const MAX_TOKENS: usize = 510;
const STYLE: usize = 256;

static VOCAB_JSON: &str = include_str!("../data/kokoro-vocab.json");

/// The voices' names as people see them (Kokoro's English voices).
pub const VOICE_NAMES: &[(&str, &str)] = &[
    ("af_heart", "Heart (US, female)"),
    ("af_bella", "Bella (US, female)"),
    ("af_nicole", "Nicole (US, female)"),
    ("af_sarah", "Sarah (US, female)"),
    ("af_sky", "Sky (US, female)"),
    ("af_nova", "Nova (US, female)"),
    ("am_michael", "Michael (US, male)"),
    ("am_adam", "Adam (US, male)"),
    ("am_eric", "Eric (US, male)"),
    ("am_liam", "Liam (US, male)"),
    ("am_onyx", "Onyx (US, male)"),
    ("bf_emma", "Emma (UK, female)"),
    ("bf_isabella", "Isabella (UK, female)"),
    ("bm_george", "George (UK, male)"),
    ("bm_daniel", "Daniel (UK, male)"),
];

pub struct Kokoro {
    model: rten::Model,
    voices: HashMap<String, Vec<f32>>,
    vocab: HashMap<char, i32>,
    us: misaki_rs::G2P,
    gb: misaki_rs::G2P,
}

/// Every array in an uncompressed NumPy .npz (a zip of .npy files): name → its float32 values.
fn read_npz(bytes: &[u8]) -> Result<HashMap<String, Vec<f32>>, String> {
    let bad = || "the voices file isn't one Eudora can read".to_string();
    let u16_at = |i: usize| bytes.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |i: usize| bytes.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let mut out = HashMap::new();
    let mut at = 0usize;
    while u32_at(at) == Some(0x0403_4b50) {
        let method = u16_at(at + 8).ok_or_else(bad)?;
        let mut size = u32_at(at + 18).ok_or_else(bad)?;
        let name_len = u16_at(at + 26).ok_or_else(bad)?;
        let extra_len = u16_at(at + 28).ok_or_else(bad)?;
        let name_at = at + 30;
        let name = String::from_utf8_lossy(bytes.get(name_at..name_at + name_len).ok_or_else(bad)?).to_string();
        let extra = bytes.get(name_at + name_len..name_at + name_len + extra_len).ok_or_else(bad)?;
        if size == 0xffff_ffff {
            // (zip64: the real size in the extra field)
            let mut i = 0;
            while i + 4 <= extra.len() {
                let id = u16::from_le_bytes([extra[i], extra[i + 1]]);
                let len = u16::from_le_bytes([extra[i + 2], extra[i + 3]]) as usize;
                if id == 1 && i + 12 <= extra.len() {
                    size = u64::from_le_bytes(extra[i + 4..i + 12].try_into().map_err(|_| bad())?) as usize;
                }
                i += 4 + len;
            }
        }
        if method != 0 {
            return Err("the voices file is compressed; Eudora reads the uncompressed one".into());
        }
        let start = name_at + name_len + extra_len;
        let data = bytes.get(start..start + size).ok_or_else(bad)?;
        at = start + size;
        // .npy: magic, version, header length, a Python dict, then the values
        if data.get(..6) != Some(b"\x93NUMPY") {
            continue;
        }
        let (hlen, hstart) = if data[6] == 1 { (u16::from_le_bytes([data[8], data[9]]) as usize, 10) } else { (u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize, 12) };
        let header = String::from_utf8_lossy(data.get(hstart..hstart + hlen).ok_or_else(bad)?);
        if !header.contains("<f4") {
            continue;
        }
        let values: Vec<f32> = data[hstart + hlen..].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        out.insert(name.trim_end_matches(".npy").to_string(), values);
    }
    if out.is_empty() {
        return Err(bad());
    }
    Ok(out)
}

impl Kokoro {
    /// Loads the model and voices from `dir` (about a second: the model is 310 MB).
    pub fn load(dir: &Path) -> Result<Kokoro, String> {
        let model = rten::Model::load_file(dir.join("kokoro-v1.0.onnx")).map_err(|e| format!("the voice model: {e}"))?;
        let voices = read_npz(&std::fs::read(dir.join("voices-v1.0.bin")).map_err(|e| format!("the voices: {e}"))?)?;
        let raw: HashMap<String, i64> = serde_json::from_str(VOCAB_JSON).map_err(|e| e.to_string())?;
        let vocab = raw.into_iter().filter_map(|(k, v)| Some((k.chars().next()?, v as i32))).collect();
        let fallback = || Some(Box::new(crate::words::Words) as Box<dyn misaki_rs::Fallback>);
        Ok(Kokoro {
            model,
            voices,
            vocab,
            us: misaki_rs::G2P::with_fallback(misaki_rs::Language::EnglishUS, fallback()),
            gb: misaki_rs::G2P::with_fallback(misaki_rs::Language::EnglishGB, fallback()),
        })
    }

    /// The voices this model has (as named in `VOICE_NAMES`, those first).
    pub fn voices(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = VOICE_NAMES.iter().filter(|(id, _)| self.voices.contains_key(*id)).map(|(id, name)| (id.to_string(), name.to_string())).collect();
        let mut others: Vec<&String> = self.voices.keys().filter(|k| !VOICE_NAMES.iter().any(|(id, _)| id == k)).collect();
        others.sort();
        out.extend(others.into_iter().map(|k| (k.clone(), k.clone())));
        out
    }

    /// A piece of text in Kokoro's phonemes (British voices read it the British way).
    pub fn phonemes(&self, text: &str, voice: &str) -> String {
        let g2p = if voice.starts_with('b') { &self.gb } else { &self.us };
        g2p.g2p(text).map(|(p, _)| p).unwrap_or_default()
    }

    /// Speech for one piece of text: its samples (−1.0 to 1.0) at `SAMPLE_RATE`.
    pub fn speak(&self, text: &str, voice: &str, speed: f32) -> Result<Vec<f32>, String> {
        let phonemes = self.phonemes(text, voice);
        let mut ids: Vec<i32> = phonemes.chars().filter_map(|c| self.vocab.get(&c).copied()).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        ids.truncate(MAX_TOKENS);
        let table = self.voices.get(voice).or_else(|| self.voices.get("af_heart")).ok_or("no voice")?;
        // (the voice's style for a text of this length: one row per token count)
        let rows = table.len() / STYLE;
        let row = ids.len().min(rows.saturating_sub(1));
        let style = table[row * STYLE..row * STYLE + STYLE].to_vec();
        let mut tokens = Vec::with_capacity(ids.len() + 2);
        tokens.push(0);
        tokens.extend(ids);
        tokens.push(0);
        let n = tokens.len();
        let input = |name: &str| self.model.node_id(name).map_err(|e| e.to_string());
        let inputs = || -> Result<Vec<(rten::NodeId, rten::ValueOrView<'static>)>, String> {
            Ok(vec![
                (input("tokens")?, NdTensor::from_data([1, n], tokens.clone()).into()),
                (input("style")?, NdTensor::from_data([1, STYLE], style.clone()).into()),
                (input("speed")?, NdTensor::from_data([1], vec![speed.clamp(0.5, 2.0)]).into()),
            ])
        };
        let audio_id = input("audio")?;
        // The vocoder takes the phase of its source's STFT as atan2(imag, real), exported as
        // imag / real with no case for 0 / 0; where rten's STFT gives exactly 0 for both, that's
        // NaN, which spreads until every sample is NaN: silence (found 2026-11-14, when Kokoro
        // still made no sound). So: the STFT first, those exact zeros nudged to a tiny real part (the
        // phase then comes out 0, as atan2(0, 0) does), then the rest from there.
        if let Some(stft_id) = self.model.find_node(STFT_NODE) {
            let first = self.model.run(inputs()?, &[stft_id], None).map_err(|e| e.to_string())?;
            let stft: Tensor<f32> = first.into_iter().next().ok_or("no STFT")?.try_into().map_err(|_| "an STFT of another kind".to_string())?;
            let shape = stft.shape().to_vec();
            let mut data = stft.to_vec();
            for pair in data.chunks_exact_mut(2) {
                if pair[0] == 0.0 && pair[1] == 0.0 {
                    pair[0] = 1e-12;
                }
            }
            let mut rest = inputs()?;
            rest.push((stft_id, Tensor::from_data(shape.as_slice(), data).into()));
            let result = self.model.run(rest, &[audio_id], None).map_err(|e| e.to_string())?;
            let audio: Tensor<f32> = result.into_iter().next().ok_or("no audio")?.try_into().map_err(|_| "audio of another kind".to_string())?;
            return Ok(audio.to_vec());
        }
        let result = self.model.run(inputs()?, &[audio_id], None).map_err(|e| e.to_string())?;
        let audio: Tensor<f32> = result.into_iter().next().ok_or("no audio")?.try_into().map_err(|_| "audio of another kind".to_string())?;
        Ok(audio.to_vec())
    }
}

/// Text cut into pieces Kokoro takes at once: sentences, and long sentences at commas, then at
/// spaces (each piece well under its 510 tokens).
pub fn pieces(text: &str) -> Vec<String> {
    const MOST: usize = 300; // characters: about 300 phonemes, under the limit with room
    let mut out = Vec::new();
    let mut piece = String::new();
    let flush = |piece: &mut String, out: &mut Vec<String>| {
        let t = piece.trim();
        if t.chars().any(|c| c.is_alphanumeric()) {
            out.push(t.to_string());
        }
        piece.clear();
    };
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        piece.push(if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c });
        let next = chars.get(i + 1).copied().unwrap_or(' ');
        let end = matches!(c, '.' | '!' | '?' | ';') && next.is_whitespace() || c == '\n' && chars.get(i + 1) == Some(&'\n');
        if end || (piece.len() >= MOST && (c == ',' || c == ' ')) || piece.len() >= MOST * 2 {
            flush(&mut piece, &mut out);
        }
    }
    flush(&mut piece, &mut out);
    out
}

/// Samples as a 16-bit mono WAV file.
pub fn wav(samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + samples.len() * 2);
    w.extend(b"RIFF");
    w.extend((36 + data_len).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(SAMPLE_RATE.to_le_bytes());
    w.extend((SAMPLE_RATE * 2).to_le_bytes());
    w.extend(2u16.to_le_bytes());
    w.extend(16u16.to_le_bytes());
    w.extend(b"data");
    w.extend(data_len.to_le_bytes());
    for s in samples {
        w.extend(((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    w
}

#[cfg(test)]
mod tests {
    #[test]
    fn text_is_cut_into_sentences_and_long_ones_at_commas() {
        let p = super::pieces("Hello there. How are you?\n\nFine!");
        assert_eq!(p, vec!["Hello there.", "How are you?", "Fine!"]);
        let long = "word, ".repeat(100);
        assert!(super::pieces(&long).iter().all(|p| p.len() <= 600));
        assert!(super::pieces("  ...  ").is_empty());
    }
}
