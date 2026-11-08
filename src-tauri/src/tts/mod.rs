//! Read aloud: select text anywhere, press the hotkey, hear it spoken with
//! Kokoro. Press the hotkey again to stop.
//!
//! Synthesis runs one sentence at a time and queues each into the audio
//! player as soon as it's ready, so speech starts after the first sentence
//! rather than after the whole selection.

mod alert;
mod cmudict;
mod fallback;
pub mod kokoro;
mod rules;
pub mod selection;
pub mod text;

use crate::managers::model::ModelManager;
use crate::notices::{self, Notice, NoticeCode, NoticeSource};
use crate::settings;
use kokoro::Kokoro;
use log::{debug, error, info};
use rodio::buffer::SamplesBuffer;
use rodio::Player;
use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

pub struct TtsManager {
    app: AppHandle,
    /// Loaded at startup once the voice is downloaded (see `preload`), or
    /// on first use, and kept: loading takes about 2 s.
    engine: Arc<Mutex<Option<Kokoro>>>,
    /// Bumped by every speak/stop; a reading stops once it no longer matches.
    generation: Arc<AtomicU64>,
    speaking: Arc<AtomicBool>,
    player: Arc<Mutex<Option<Arc<Player>>>>,
}

impl TtsManager {
    pub fn new(app: &AppHandle) -> Self {
        Self {
            app: app.clone(),
            engine: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            speaking: Arc::new(AtomicBool::new(false)),
            player: Arc::new(Mutex::new(None)),
        }
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking.load(Ordering::SeqCst)
    }

    /// The hotkey: stop if reading, otherwise read the current selection.
    pub fn toggle_read_selection(&self) {
        if self.is_speaking() {
            info!("Read aloud: stopped by hotkey");
            self.stop();
            return;
        }
        let worker = self.worker();
        std::thread::spawn(move || {
            let Some(text) = selection::selected_text(&worker.app) else {
                info!("Read aloud: nothing selected");
                notices::raise_without_overlay(
                    &worker.app,
                    Notice::new(
                        NoticeCode::ReadAloudNothingSelected,
                        NoticeSource::ReadAloud,
                    ),
                );
                // The notice only shows in Handy's window; the user is
                // looking at another app, so tell them by sound too.
                alert::play_nothing_selected(&worker.app);
                return;
            };
            worker.speak(text);
        });
    }

    /// Read `text` aloud, replacing anything currently being read.
    pub fn speak_text(&self, text: String) {
        let worker = self.worker();
        std::thread::spawn(move || worker.speak(text));
    }

    /// Load the voice in the background if it's downloaded, so the first
    /// read starts as fast as later ones. A read that arrives meanwhile
    /// waits for the load instead of starting a second one.
    pub fn preload(&self) {
        let worker = self.worker();
        std::thread::spawn(move || {
            let mut engine = worker.engine.lock().unwrap();
            if engine.is_some() {
                return;
            }
            match worker.load_engine(&mut engine) {
                Ok(true) => {
                    // ONNX Runtime's first run is slower than later ones.
                    let warm = Instant::now();
                    let kokoro = engine.as_mut().expect("loaded above");
                    let warmed =
                        kokoro
                            .tokenize("Ready.")
                            .and_then(|chunks| match chunks.first() {
                                Some(ids) => kokoro.synthesize(ids, 1.0).map(|_| ()),
                                None => Ok(()),
                            });
                    match warmed {
                        Ok(()) => info!("Read aloud: warmed up in {:?}", warm.elapsed()),
                        Err(e) => error!("Read aloud: warm-up failed: {e:#}"),
                    }
                }
                Ok(false) => debug!("Read aloud: voice not downloaded; nothing to preload"),
                Err(e) => error!("Read aloud: preloading the voice failed: {e:#}"),
            }
        });
    }

    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.speaking.store(false, Ordering::SeqCst);
        if let Some(player) = self.player.lock().unwrap().take() {
            player.stop();
        }
    }

    fn worker(&self) -> Worker {
        Worker {
            app: self.app.clone(),
            engine: self.engine.clone(),
            generation: self.generation.clone(),
            speaking: self.speaking.clone(),
            player: self.player.clone(),
        }
    }
}

/// The parts of the manager a background reading needs.
struct Worker {
    app: AppHandle,
    engine: Arc<Mutex<Option<Kokoro>>>,
    generation: Arc<AtomicU64>,
    speaking: Arc<AtomicBool>,
    player: Arc<Mutex<Option<Arc<Player>>>>,
}

impl Worker {
    /// Load Kokoro into `engine`. `Ok(false)` if the voice isn't downloaded.
    fn load_engine(&self, engine: &mut Option<Kokoro>) -> anyhow::Result<bool> {
        let Some((model, voice)) = self.app.state::<Arc<ModelManager>>().tts_model_paths() else {
            return Ok(false);
        };
        let load = Instant::now();
        *engine = Some(Kokoro::load(&model, &voice)?);
        info!("Read aloud: loaded Kokoro in {:?}", load.elapsed());
        Ok(true)
    }

    fn speak(&self, raw: String) {
        // Claim this reading; any earlier one sees the new generation and stops.
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(old) = self.player.lock().unwrap().take() {
            old.stop();
        }
        let current = || self.generation.load(Ordering::SeqCst) == generation;

        let sentences = text::sentences(&text::clean(&raw));
        if sentences.is_empty() {
            return;
        }
        self.speaking.store(true, Ordering::SeqCst);
        let started = Instant::now();

        if let Err(e) = self.read_sentences(&sentences, &current, started) {
            error!("Read aloud failed: {e:#}");
            let code = if e.downcast_ref::<VoiceMissing>().is_some() {
                NoticeCode::ReadAloudVoiceMissing
            } else {
                NoticeCode::ReadAloudFailed
            };
            notices::raise_without_overlay(
                &self.app,
                Notice::new(code, NoticeSource::ReadAloud).with_detail(format!("{e:#}")),
            );
        }

        if current() {
            self.speaking.store(false, Ordering::SeqCst);
            self.player.lock().unwrap().take();
        }
    }

    fn read_sentences(
        &self,
        sentences: &[String],
        current: &dyn Fn() -> bool,
        started: Instant,
    ) -> anyhow::Result<()> {
        let speed = settings::get_settings(&self.app).read_aloud_speed;
        let mut engine = self.engine.lock().unwrap();
        if engine.is_none() && !self.load_engine(&mut engine)? {
            return Err(VoiceMissing.into());
        }
        let engine = engine.as_mut().expect("loaded above");

        // The output stream has to stay alive on this thread while it plays.
        let output = crate::audio_feedback::open_output_stream(&self.app)
            .map_err(|e| anyhow::anyhow!("opening the audio output: {e}"))?;
        let player = Arc::new(Player::connect_new(output.mixer()));
        *self.player.lock().unwrap() = Some(player.clone());
        if !current() {
            return Ok(());
        }
        notices::clear(&self.app, NoticeSource::ReadAloud);

        for (i, sentence) in sentences.iter().enumerate() {
            for chunk in engine.tokenize(sentence)? {
                if !current() {
                    return Ok(());
                }
                let audio = engine.synthesize(&chunk, speed)?;
                if i == 0 {
                    debug!("Read aloud: first audio after {:?}", started.elapsed());
                }
                player.append(SamplesBuffer::new(
                    NonZero::new(1).unwrap(),
                    NonZero::new(kokoro::SAMPLE_RATE).unwrap(),
                    audio,
                ));
            }
        }

        while current() && !player.empty() {
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct VoiceMissing;

impl std::fmt::Display for VoiceMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the read-aloud voice hasn't been downloaded")
    }
}

impl std::error::Error for VoiceMissing {}
