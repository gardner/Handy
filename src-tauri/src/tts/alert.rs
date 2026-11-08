//! The tone played when the Read Aloud shortcut can't read anything (nothing
//! selected, or no voice yet), so a press is never silent. Synthesized rather than shipped as a file: two soft
//! falling notes, distinct from the dictation start/stop sounds.

use crate::settings;
use rodio::buffer::SamplesBuffer;
use rodio::Player;
use std::num::NonZero;
use std::time::Duration;
use tauri::AppHandle;

const RATE: u32 = 44_100;

/// Play the alert and return once it has finished (call off the UI thread).
pub fn play(app: &AppHandle) {
    let settings = settings::get_settings(app);
    if !settings.read_aloud_alert_sound {
        return;
    }
    let output = match crate::audio_feedback::open_output_stream(app) {
        Ok(output) => output,
        Err(e) => {
            log::warn!("Read aloud: couldn't play the alert tone: {e}");
            return;
        }
    };
    let player = Player::connect_new(output.mixer());
    player.set_volume(settings.audio_feedback_volume.clamp(0.0, 1.0));
    player.append(SamplesBuffer::new(
        NonZero::new(1).unwrap(),
        NonZero::new(RATE).unwrap(),
        tone(),
    ));
    while !player.empty() {
        std::thread::sleep(Duration::from_millis(20));
    }
    // Let the device drain its buffer before the stream is dropped.
    std::thread::sleep(Duration::from_millis(80));
}

/// E5 then C5, each with a quick attack and a bell-like decay.
fn tone() -> Vec<f32> {
    let mut out = Vec::new();
    for (freq, secs) in [(659.25_f32, 0.13_f32), (523.25, 0.22)] {
        let n = (secs * RATE as f32) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let attack = (t / 0.008).min(1.0);
            let decay = (-t * 14.0).exp();
            let phase = std::f32::consts::TAU * freq * t;
            // A little second harmonic makes it read as a chime, not a beep.
            let wave = phase.sin() + 0.25 * (2.0 * phase).sin();
            out.push(0.35 * attack * decay * wave);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn tone_is_short_and_never_clips() {
        let tone = super::tone();
        let secs = tone.len() as f32 / super::RATE as f32;
        assert!((0.3..0.5).contains(&secs));
        assert!(tone.iter().all(|s| s.abs() < 1.0));
    }
}
