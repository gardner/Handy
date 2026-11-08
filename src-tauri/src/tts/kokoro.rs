//! Kokoro-82M text-to-speech through ONNX Runtime.
//!
//! Text → misaki phonemes (English, MIT, no espeak-ng) → Kokoro token ids →
//! 24 kHz mono audio. The model and voice files are downloaded on demand (see
//! `ModelManager::download_tts_model`); the phoneme vocabulary is tiny and
//! compiled in.

use super::fallback::SegmentingFallback;
use anyhow::{anyhow, Context, Result};
use misaki_rs::{Language, G2P};
use ort::session::Session;
use ort::value::Tensor;
use std::collections::HashMap;
use std::path::Path;

pub const SAMPLE_RATE: u32 = 24_000;
/// Kokoro's context is 512 tokens including the two boundary pads.
const MAX_TOKENS: usize = 510;
/// Each voice file is a [510, 1, 256] f32 table of style vectors, one row per
/// input length.
const STYLE_DIM: usize = 256;

/// Where Handy fetches Kokoro from: pinned to a commit so the bytes match.
pub const REPO_ID: &str = "onnx-community/Kokoro-82M-v1.0-ONNX";
pub const REVISION: &str = "1939ad2a8e416c0acfeecc08a694d14ef25f2231";
/// Full precision: the quantized variants are smaller but audibly rougher.
pub const MODEL_FILE: &str = "onnx/model.onnx";
pub const MODEL_SIZE: u64 = 325_532_232;
pub const MODEL_SHA256: &str = "8fbea51ea711f2af382e88c833d9e288c6dc82ce5e98421ea61c058ce21a34cb";
/// af_heart: American English, the highest-rated Kokoro voice.
pub const VOICE_FILE: &str = "voices/af_heart.bin";
pub const VOICE_SIZE: u64 = 522_240;
pub const VOICE_SHA256: &str = "d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b";

pub struct Kokoro {
    session: Session,
    /// Kept alive for as long as the session that was built from it.
    _env: ort::environment::Environment,
    g2p: G2P,
    vocab: HashMap<char, i64>,
    voice: Vec<f32>,
}

impl Kokoro {
    pub fn load(model_path: &Path, voice_path: &Path) -> Result<Self> {
        let vocab: HashMap<String, i64> =
            serde_json::from_str(include_str!("kokoro_vocab.json")).context("kokoro vocab")?;
        let vocab = vocab
            .into_iter()
            .filter_map(|(k, v)| Some((k.chars().next()?, v)))
            .collect();

        let bytes = std::fs::read(voice_path)
            .with_context(|| format!("reading voice {}", voice_path.display()))?;
        if bytes.len() % (STYLE_DIM * 4) != 0 {
            return Err(anyhow!(
                "voice file has an unexpected size: {}",
                bytes.len()
            ));
        }
        let voice = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();

        let env = ort::environment::init().with_name("handy-tts").build()?;
        let session = Session::builder(&env)?
            .commit_from_file(model_path)
            .with_context(|| format!("loading Kokoro from {}", model_path.display()))?;

        let g2p = G2P::with_fallback(
            Language::EnglishUS,
            Some(Box::new(SegmentingFallback::new(Language::EnglishUS))),
        );

        Ok(Self {
            session,
            _env: env,
            g2p,
            vocab,
            voice,
        })
    }

    /// Phoneme token ids for `text`, split into pieces that fit the model.
    pub fn tokenize(&self, text: &str) -> Result<Vec<Vec<i64>>> {
        let (phonemes, _) = self
            .g2p
            .g2p(text)
            .map_err(|e| anyhow!("phonemizing failed: {e:?}"))?;
        log::trace!("TTS phonemes: {phonemes}");
        let ids: Vec<i64> = phonemes
            .chars()
            .filter_map(|c| self.vocab.get(&c).copied())
            .collect();
        Ok(split_tokens(ids, self.vocab.get(&' ').copied()))
    }

    /// Synthesize one chunk of token ids (at most `MAX_TOKENS`) at `speed`
    /// (1.0 = normal).
    pub fn synthesize(&mut self, ids: &[i64], speed: f32) -> Result<Vec<f32>> {
        let n = ids.len().min(MAX_TOKENS);
        let rows = self.voice.len() / STYLE_DIM;
        let row = n.min(rows - 1);
        let style = self.voice[row * STYLE_DIM..(row + 1) * STYLE_DIM].to_vec();

        let mut input = Vec::with_capacity(n + 2);
        input.push(0);
        input.extend_from_slice(&ids[..n]);
        input.push(0);
        let len = input.len();

        let outputs = self.session.run(ort::inputs![
            "input_ids" => Tensor::from_array(([1usize, len], input))?,
            "style" => Tensor::from_array(([1usize, STYLE_DIM], style))?,
            "speed" => Tensor::from_array(([1usize], vec![speed]))?,
        ])?;
        let (_, audio) = outputs[0].try_extract_tensor::<f32>()?;
        Ok(audio.to_vec())
    }
}

/// Split token ids into chunks of at most `MAX_TOKENS`, preferring to break
/// at a space so words aren't cut in half.
fn split_tokens(ids: Vec<i64>, space: Option<i64>) -> Vec<Vec<i64>> {
    let mut chunks = Vec::new();
    let mut rest = ids.as_slice();
    while rest.len() > MAX_TOKENS {
        let cut = space
            .and_then(|s| rest[..MAX_TOKENS].iter().rposition(|&t| t == s))
            .filter(|&i| i > MAX_TOKENS / 2)
            .unwrap_or(MAX_TOKENS);
        chunks.push(rest[..cut].to_vec());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        chunks.push(rest.to_vec());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_input_is_one_chunk() {
        assert_eq!(split_tokens(vec![1, 2, 3], Some(16)), vec![vec![1, 2, 3]]);
    }

    #[test]
    fn long_input_breaks_at_a_space() {
        let mut ids = vec![50; 400];
        ids.push(16);
        ids.extend(vec![50; 300]);
        let chunks = split_tokens(ids, Some(16));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 400);
        assert!(chunks.iter().all(|c| c.len() <= MAX_TOKENS));
    }

    #[test]
    fn vocab_parses() {
        let vocab: HashMap<String, i64> =
            serde_json::from_str(include_str!("kokoro_vocab.json")).unwrap();
        assert_eq!(vocab.get("$"), Some(&0));
        assert!(vocab.len() > 100);
    }

    /// End-to-end synthesis against real model files. Run with
    /// `KOKORO_DIR=<dir with onnx/model.onnx and voices/af_heart.bin>
    /// cargo test --lib tts::kokoro -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn synthesizes_a_selection() {
        let dir = std::path::PathBuf::from(std::env::var("KOKORO_DIR").expect("KOKORO_DIR"));
        let start = std::time::Instant::now();
        let mut kokoro = Kokoro::load(&dir.join(MODEL_FILE), &dir.join(VOICE_FILE)).unwrap();
        eprintln!("load: {:?}", start.elapsed());

        let raw = "## Read Aloud\nSelect any text in Handy, press the short-\ncut, and Kubernetes \
                   docs [3] become audio. See https://handy.computer for more.\n\n- It costs $0.\n- It runs offline.";
        let sentences = crate::tts::text::sentences(&crate::tts::text::clean(raw));
        eprintln!("sentences: {sentences:#?}");

        let mut audio = Vec::new();
        let synth = std::time::Instant::now();
        for (i, sentence) in sentences.iter().enumerate() {
            for chunk in kokoro.tokenize(sentence).unwrap() {
                audio.extend(kokoro.synthesize(&chunk, 1.0).unwrap());
                if i == 0 && !audio.is_empty() {
                    eprintln!("first audio after {:?}", synth.elapsed());
                }
            }
        }
        let secs = audio.len() as f32 / SAMPLE_RATE as f32;
        eprintln!("{secs:.2}s of audio in {:?}", synth.elapsed());
        assert!(secs > 3.0);
        assert!(audio.iter().any(|s| s.abs() > 0.1), "audio is silent");

        if let Ok(out) = std::env::var("KOKORO_WAV") {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: SAMPLE_RATE,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(out, spec).unwrap();
            audio.iter().for_each(|s| w.write_sample(*s).unwrap());
            w.finalize().unwrap();
        }
    }
}
