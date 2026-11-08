//! Commands for the Read Aloud settings section.

use crate::managers::model::{ModelManager, TTS_MODEL_ID};
use crate::settings;
use crate::tts::TtsManager;
use log::error;
use serde::Serialize;
use specta::Type;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

#[derive(Serialize, Type)]
pub struct ReadAloudStatus {
    /// The Kokoro model and af_heart voice are on disk.
    pub voice_downloaded: bool,
    pub speaking: bool,
    /// Progress/cancel id for the voice download.
    pub download_id: String,
}

#[tauri::command]
#[specta::specta]
pub fn get_read_aloud_status(
    model_manager: State<'_, Arc<ModelManager>>,
    tts: State<'_, TtsManager>,
) -> ReadAloudStatus {
    ReadAloudStatus {
        voice_downloaded: model_manager.tts_model_paths().is_some(),
        speaking: tts.is_speaking(),
        download_id: TTS_MODEL_ID.to_string(),
    }
}

/// Download the read-aloud voice. Progress arrives as `model-download-progress`
/// events for `download_id`; cancel with `cancel_download(download_id)`.
#[tauri::command]
#[specta::specta]
pub async fn download_read_aloud_voice(
    app_handle: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<(), String> {
    let result = model_manager
        .download_tts_model()
        .await
        .map_err(|e| e.to_string());
    if let Err(ref e) = result {
        error!("Read-aloud voice download failed: {e}");
        let _ = app_handle.emit(
            "model-download-failed",
            serde_json::json!({ "model_id": TTS_MODEL_ID, "error": e }),
        );
    }
    result
}

/// Speak `text` (used by the settings page to preview the voice and speed).
#[tauri::command]
#[specta::specta]
pub fn read_aloud_text(tts: State<'_, TtsManager>, text: String) {
    tts.speak_text(text);
}

#[tauri::command]
#[specta::specta]
pub fn stop_read_aloud(tts: State<'_, TtsManager>) {
    tts.stop();
}

#[tauri::command]
#[specta::specta]
pub fn change_read_aloud_speed_setting(app: AppHandle, speed: f32) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.read_aloud_speed = speed.clamp(0.5, 2.0);
    settings::write_settings(&app, settings);
    Ok(())
}
