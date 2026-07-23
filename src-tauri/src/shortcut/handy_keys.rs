//! Handy-keys based keyboard shortcut implementation
//!
//! This module provides an alternative to Tauri's global-shortcut plugin
//! using the handy-keys library for more control over keyboard events.
//!
//! ## Architecture
//!
//! The implementation uses a dedicated manager thread that owns the `HotkeyManager`:
//!
//! ```text
//! ┌─────────────────┐     commands      ┌──────────────────────┐
//! │   Main Thread   │ ───────────────▶ │   Manager Thread     │
//! │                 │   (via channel)   │                      │
//! │ - register()    │                   │ - owns HotkeyManager │
//! │ - unregister()  │                   │ - polls for events   │
//! └─────────────────┘                   │ - dispatches actions │
//!                                       └──────────────────────┘
//! ```
//!
//! This design ensures thread-safety since `HotkeyManager` is only accessed
//! from a single thread. Commands (register/unregister) are sent via an mpsc
//! channel and responses are synchronously awaited.
//!
//! ## Recording Mode
//!
//! For UI key capture, a separate `KeyboardListener` is created on-demand and
//! polled from a dedicated recording thread. Events are emitted to the frontend
//! via Tauri's event system.

use handy_keys::{Hotkey, HotkeyId, HotkeyManager, HotkeyState, KeyboardListener};
use log::{debug, error, info};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tauri::{AppHandle, Emitter, Manager};

use crate::settings::{self, get_settings, ShortcutBinding};

use super::handler::handle_shortcut_event;

const MANAGER_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MANAGER_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Commands that can be sent to the hotkey manager thread
enum ManagerCommand {
    Register {
        binding_id: String,
        hotkey_string: String,
        response: Sender<Result<(), String>>,
    },
    Unregister {
        binding_id: String,
        response: Sender<Result<(), String>>,
    },
    Shutdown,
}

struct RuntimeSlot<T> {
    active: Option<T>,
}

impl<T> Default for RuntimeSlot<T> {
    fn default() -> Self {
        Self { active: None }
    }
}

impl<T> RuntimeSlot<T> {
    fn install(&mut self, runtime: T) -> Result<(), T> {
        if self.active.is_some() {
            return Err(runtime);
        }
        self.active = Some(runtime);
        Ok(())
    }

    fn get(&self) -> Option<&T> {
        self.active.as_ref()
    }

    fn get_mut(&mut self) -> Option<&mut T> {
        self.active.as_mut()
    }

    fn take(&mut self) -> Option<T> {
        self.active.take()
    }

    fn is_running(&self) -> bool {
        self.active.is_some()
    }
}

fn take_runtime<T>(slot: &Mutex<RuntimeSlot<T>>) -> Result<Option<T>, String> {
    slot.lock()
        .map_err(|_| "Failed to lock handy-keys runtime".to_string())
        .map(|mut slot| slot.take())
}

/// Permanently managed controller for the restartable handy-keys runtime.
///
/// Tauri managed state itself cannot be safely removed at runtime. Keeping the
/// listener inside an `Option` lets us drop the actual event-tap owner whenever
/// the user switches to the permission-free Tauri backend.
pub struct HandyKeysState {
    runtime: Mutex<RuntimeSlot<HandyKeysRuntime>>,
}

impl Default for HandyKeysState {
    fn default() -> Self {
        Self {
            runtime: Mutex::new(RuntimeSlot::default()),
        }
    }
}

/// Active handy-keys listener and manager thread.
struct HandyKeysRuntime {
    /// Channel to send commands to the manager thread (wrapped in Mutex for Sync)
    command_sender: Mutex<Sender<ManagerCommand>>,
    /// Handle to the manager thread, joined when the runtime is stopped.
    thread_handle: Option<JoinHandle<()>>,
    /// Recording listener for UI key capture (only active during recording)
    recording_listener: Mutex<Option<KeyboardListener>>,
    /// Flag indicating if we're in recording mode
    is_recording: AtomicBool,
    /// The binding ID being recorded (if any)
    recording_binding_id: Mutex<Option<String>>,
    /// Flag to stop recording loop
    recording_running: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    accessibility_monitor_running: Arc<AtomicBool>,
}

/// Key event sent to frontend during recording mode
#[derive(Debug, Clone, Serialize, Type)]
pub struct FrontendKeyEvent {
    /// Currently pressed modifier keys
    pub modifiers: Vec<String>,
    /// The key that was pressed (if any)
    pub key: Option<String>,
    /// Whether this is a key down event
    pub is_key_down: bool,
    /// The full hotkey string (e.g., "option+space")
    pub hotkey_string: String,
}

impl HandyKeysRuntime {
    /// Create a runtime and wait until its native listener is ready.
    fn new(app: AppHandle) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ManagerCommand>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let app_clone = app.clone();
        let thread_handle = thread::spawn(move || {
            Self::manager_thread(cmd_rx, app_clone, ready_tx);
        });

        match ready_rx.recv_timeout(MANAGER_STARTUP_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = thread_handle.join();
                return Err(error);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = thread_handle.join();
                return Err("handy-keys manager exited before initialization completed".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Dropping the handle detaches a constructor that is stuck in
                // native code; dropping cmd_tx below makes it exit if it later
                // reaches the command loop.
                return Err("Timed out while initializing the handy-keys manager".into());
            }
        }

        Ok(Self {
            command_sender: Mutex::new(cmd_tx),
            thread_handle: Some(thread_handle),
            recording_listener: Mutex::new(None),
            is_recording: AtomicBool::new(false),
            recording_binding_id: Mutex::new(None),
            recording_running: Arc::new(AtomicBool::new(false)),
            #[cfg(target_os = "macos")]
            accessibility_monitor_running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The main manager thread - owns the HotkeyManager and processes commands
    fn manager_thread(
        cmd_rx: Receiver<ManagerCommand>,
        app: AppHandle,
        ready: mpsc::SyncSender<Result<(), String>>,
    ) {
        info!("handy-keys manager thread started");

        // Create the HotkeyManager in this thread
        let manager = match HotkeyManager::new_with_blocking() {
            Ok(m) => m,
            Err(e) => {
                let error = format!("Failed to create HotkeyManager: {}", e);
                error!("{}", error);
                let _ = ready.send(Err(error));
                return;
            }
        };
        if ready.send(Ok(())).is_err() {
            return;
        }

        // Maps binding IDs to HotkeyIds and hotkey strings
        let mut binding_to_hotkey: HashMap<String, HotkeyId> = HashMap::new();
        let mut hotkey_to_binding: HashMap<HotkeyId, (String, String)> = HashMap::new(); // (binding_id, hotkey_string)

        loop {
            // Check for hotkey events (non-blocking)
            while let Some(event) = manager.try_recv() {
                if let Some((binding_id, hotkey_string)) = hotkey_to_binding.get(&event.id) {
                    debug!(
                        "handy-keys event: binding={}, hotkey={}, state={:?}",
                        binding_id, hotkey_string, event.state
                    );
                    let is_pressed = event.state == HotkeyState::Pressed;
                    handle_shortcut_event(&app, binding_id, hotkey_string, is_pressed);
                }
            }

            // Check for commands (non-blocking with timeout)
            match cmd_rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(cmd) => match cmd {
                    ManagerCommand::Register {
                        binding_id,
                        hotkey_string,
                        response,
                    } => {
                        let result = Self::do_register(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                            &hotkey_string,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Unregister {
                        binding_id,
                        response,
                    } => {
                        let result = Self::do_unregister(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Shutdown => {
                        info!("handy-keys manager thread shutting down");
                        break;
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No command, continue
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    info!("Command channel disconnected, shutting down");
                    break;
                }
            }
        }

        info!("handy-keys manager thread stopped");
    }

    /// Register a hotkey
    fn do_register(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
        hotkey_string: &str,
    ) -> Result<(), String> {
        let hotkey: Hotkey = hotkey_string
            .parse()
            .map_err(|e| format!("Failed to parse hotkey '{}': {}", hotkey_string, e))?;

        let id = manager
            .register(hotkey)
            .map_err(|e| format!("Failed to register hotkey: {}", e))?;

        binding_to_hotkey.insert(binding_id.to_string(), id);
        hotkey_to_binding.insert(id, (binding_id.to_string(), hotkey_string.to_string()));

        debug!(
            "Registered handy-keys shortcut: {} -> {:?}",
            binding_id, hotkey
        );
        Ok(())
    }

    /// Unregister a hotkey
    fn do_unregister(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
    ) -> Result<(), String> {
        if let Some(id) = binding_to_hotkey.remove(binding_id) {
            manager
                .unregister(id)
                .map_err(|e| format!("Failed to unregister hotkey: {}", e))?;
            hotkey_to_binding.remove(&id);
            debug!("Unregistered handy-keys shortcut: {}", binding_id);
        }
        Ok(())
    }

    /// Register a shortcut binding
    pub fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.command_sender
            .lock()
            .map_err(|_| "Failed to lock command_sender")?
            .send(ManagerCommand::Register {
                binding_id: binding.id.clone(),
                hotkey_string: binding.current_binding.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send register command")?;

        rx.recv_timeout(MANAGER_COMMAND_TIMEOUT)
            .map_err(|error| format!("Failed to receive register response: {error}"))?
    }

    /// Unregister a shortcut binding
    pub fn unregister(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.command_sender
            .lock()
            .map_err(|_| "Failed to lock command_sender")?
            .send(ManagerCommand::Unregister {
                binding_id: binding.id.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send unregister command")?;

        rx.recv_timeout(MANAGER_COMMAND_TIMEOUT)
            .map_err(|error| format!("Failed to receive unregister response: {error}"))?
    }

    /// Start recording mode for a specific binding
    pub fn start_recording(&self, app: &AppHandle, binding_id: String) -> Result<(), String> {
        if self.is_recording.load(Ordering::SeqCst) {
            return Err("Already recording".into());
        }

        // Create a new keyboard listener for recording
        let listener = KeyboardListener::new()
            .map_err(|e| format!("Failed to create keyboard listener: {}", e))?;

        {
            let mut recording = self
                .recording_listener
                .lock()
                .map_err(|_| "Failed to lock recording_listener")?;
            *recording = Some(listener);
        }
        {
            let mut binding = self
                .recording_binding_id
                .lock()
                .map_err(|_| "Failed to lock recording_binding_id")?;
            *binding = Some(binding_id);
        }

        self.is_recording.store(true, Ordering::SeqCst);
        self.recording_running.store(true, Ordering::SeqCst);

        // Start a thread to emit key events to the frontend
        let app_clone = app.clone();
        let recording_running = Arc::clone(&self.recording_running);
        thread::spawn(move || {
            HandyKeysState::recording_loop(app_clone, recording_running);
        });

        debug!("Started handy-keys recording mode");
        Ok(())
    }

    /// Stop recording mode
    pub fn stop_recording(&self) -> Result<(), String> {
        self.is_recording.store(false, Ordering::SeqCst);
        self.recording_running.store(false, Ordering::SeqCst);

        {
            let mut recording = self
                .recording_listener
                .lock()
                .map_err(|_| "Failed to lock recording_listener")?;
            *recording = None;
        }
        {
            let mut binding = self
                .recording_binding_id
                .lock()
                .map_err(|_| "Failed to lock recording_binding_id")?;
            *binding = None;
        }

        debug!("Stopped handy-keys recording mode");
        Ok(())
    }
}

impl Drop for HandyKeysRuntime {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.accessibility_monitor_running
                .store(false, Ordering::SeqCst);
        }

        // Signal recording to stop
        self.recording_running.store(false, Ordering::SeqCst);
        self.is_recording.store(false, Ordering::SeqCst);

        // Send shutdown command
        if let Ok(sender) = self.command_sender.lock() {
            let _ = sender.send(ManagerCommand::Shutdown);
        }

        // Wait for the manager thread to finish
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

impl HandyKeysState {
    fn start(&self, app: &AppHandle) -> Result<(), String> {
        {
            let runtime = self
                .runtime
                .lock()
                .map_err(|_| "Failed to lock handy-keys runtime")?;
            if runtime.is_running() {
                return Ok(());
            }
        }

        // Build and register everything before publishing the runtime. Any
        // failure drops this local value and tears down its native listener.
        let runtime = HandyKeysRuntime::new(app.clone())?;
        let default_bindings = settings::get_default_settings().bindings;
        let user_settings = settings::load_or_create_app_settings(app);

        for (id, default_binding) in default_bindings {
            if id == "cancel"
                || (id == "transcribe_with_post_process" && !user_settings.post_process_enabled)
            {
                continue;
            }

            let binding = user_settings
                .bindings
                .get(&id)
                .cloned()
                .unwrap_or(default_binding);
            runtime
                .register(&binding)
                .map_err(|error| format!("Failed to register shortcut '{}': {}", id, error))?;
        }

        self.runtime
            .lock()
            .map_err(|_| "Failed to lock handy-keys runtime")?
            .install(runtime)
            .map_err(|_| "handy-keys runtime was initialized concurrently".to_string())?;

        #[cfg(target_os = "macos")]
        if let Err(error) = self.start_accessibility_monitor(app) {
            let _ = self.stop();
            return Err(error);
        }

        Ok(())
    }

    pub fn stop(&self) -> Result<bool, String> {
        // Take ownership while locked, then release the controller mutex before
        // Drop joins worker threads. The revocation monitor may itself be
        // waiting on this mutex to discover that another path already stopped.
        let runtime = take_runtime(&self.runtime)?;
        let stopped = runtime.is_some();
        drop(runtime);
        Ok(stopped)
    }

    pub fn is_running(&self) -> bool {
        self.runtime
            .lock()
            .is_ok_and(|runtime| runtime.is_running())
    }

    pub fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock handy-keys runtime")?;
        runtime
            .get()
            .ok_or_else(|| "HandyKeys runtime is not active".to_string())?
            .register(binding)
    }

    pub fn unregister(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock handy-keys runtime")?;
        runtime
            .get()
            .ok_or_else(|| "HandyKeys runtime is not active".to_string())?
            .unregister(binding)
    }

    pub fn start_recording(&self, app: &AppHandle, binding_id: String) -> Result<(), String> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock handy-keys runtime")?;
        runtime
            .get()
            .ok_or_else(|| "HandyKeys runtime is not active".to_string())?
            .start_recording(app, binding_id)
    }

    pub fn stop_recording(&self) -> Result<(), String> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock handy-keys runtime")?;
        runtime
            .get()
            .ok_or_else(|| "HandyKeys runtime is not active".to_string())?
            .stop_recording()
    }

    fn next_recording_event(&self) -> Option<handy_keys::KeyEvent> {
        let runtime = self.runtime.lock().ok()?;
        let listener = runtime.get()?.recording_listener.lock().ok()?;
        listener.as_ref()?.try_recv()
    }

    #[cfg(target_os = "macos")]
    fn start_accessibility_monitor(&self, app: &AppHandle) -> Result<(), String> {
        let mut slot = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock handy-keys runtime")?;
        let runtime = slot
            .get_mut()
            .ok_or_else(|| "HandyKeys runtime is not active".to_string())?;

        runtime
            .accessibility_monitor_running
            .store(true, Ordering::SeqCst);
        let running = Arc::clone(&runtime.accessibility_monitor_running);
        let app = app.clone();
        // Detached deliberately: Drop signals `running` but never joins this
        // monitor, because it may be waiting for the serialized transition
        // currently dropping the runtime.
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                if runtime_health_action(true, ::handy_keys::check_accessibility())
                    == RuntimeHealthAction::StopAndFallback
                {
                    error!(
                        "macOS Accessibility was revoked while HandyKeys was active; \
                         stopping the event tap"
                    );
                    super::handle_accessibility_revoked(&app);
                    break;
                }
                // Published handy-keys 0.3.1 checks its disabled tap roughly
                // every 100 ms. Poll faster as defense in depth until the
                // library itself can atomically refuse re-enable on revoke.
                thread::sleep(std::time::Duration::from_millis(25));
            }
        });
        Ok(())
    }

    /// Recording loop - emits key events to frontend during recording.
    fn recording_loop(app: AppHandle, running: Arc<AtomicBool>) {
        while running.load(Ordering::SeqCst) {
            let event = app
                .try_state::<HandyKeysState>()
                .and_then(|state| state.next_recording_event());

            if let Some(key_event) = event {
                let frontend_event = FrontendKeyEvent {
                    modifiers: modifiers_to_strings(key_event.modifiers),
                    key: key_event.key.map(|key| key.to_string().to_lowercase()),
                    is_key_down: key_event.is_key_down,
                    hotkey_string: key_event
                        .as_hotkey()
                        .map(|hotkey| hotkey.to_handy_string())
                        .unwrap_or_default(),
                };

                if let Err(error) = app.emit("handy-keys-event", &frontend_event) {
                    error!("Failed to emit key event: {}", error);
                }
            } else {
                thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        debug!("Recording loop ended");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeHealthAction {
    Continue,
    StopAndFallback,
}

fn runtime_health_action(
    runtime_running: bool,
    accessibility_granted: bool,
) -> RuntimeHealthAction {
    if runtime_running && !accessibility_granted {
        RuntimeHealthAction::StopAndFallback
    } else {
        RuntimeHealthAction::Continue
    }
}

/// Convert handy-keys Modifiers to a list of strings
fn modifiers_to_strings(modifiers: handy_keys::Modifiers) -> Vec<String> {
    let mut result = Vec::new();

    if modifiers.contains(handy_keys::Modifiers::CTRL) {
        result.push("ctrl".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::OPT) {
        #[cfg(target_os = "macos")]
        result.push("option".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("alt".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::SHIFT) {
        result.push("shift".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::CMD) {
        #[cfg(target_os = "macos")]
        result.push("command".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("super".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::FN) {
        result.push("fn".to_string());
    }

    result
}

/// Validate a shortcut string for the HandyKeys implementation.
/// HandyKeys is more permissive: allows modifier-only combos and the fn key.
pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("Shortcut cannot be empty".into());
    }
    // HandyKeys accepts modifier-only, key-only, and modifier+key combos
    // Just verify the string is parseable
    raw.parse::<Hotkey>()
        .map(|_| ())
        .map_err(|e| format!("Invalid shortcut for HandyKeys: {}", e))
}

/// Initialize handy-keys shortcuts
pub fn init_shortcuts(app: &AppHandle) -> Result<(), String> {
    super::require_macos_accessibility("advanced keyboard shortcuts")?;

    if app.try_state::<HandyKeysState>().is_none() {
        app.manage(HandyKeysState::default());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("Failed to manage HandyKeysState")?;
    state.start(app)?;
    info!("handy-keys shortcuts initialized");
    Ok(())
}

/// Stop and drop the active listener, including its native macOS event tap.
pub fn shutdown(app: &AppHandle) -> Result<bool, String> {
    app.try_state::<HandyKeysState>()
        .map_or(Ok(false), |state| state.stop())
}

pub fn is_running(app: &AppHandle) -> bool {
    app.try_state::<HandyKeysState>()
        .is_some_and(|state| state.is_running())
}

/// Register a shortcut
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.register(&binding)
}

/// Unregister a shortcut
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.unregister(&binding)
}

/// Start key recording mode
#[tauri::command]
#[specta::specta]
pub fn start_handy_keys_recording(app: AppHandle, binding_id: String) -> Result<(), String> {
    super::require_macos_accessibility("advanced keyboard shortcut recording")?;

    let settings = get_settings(&app);
    if settings.keyboard_implementation != settings::KeyboardImplementation::HandyKeys {
        return Err("handy-keys is not the active keyboard implementation".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.start_recording(&app, binding_id)
}

/// Stop key recording mode
#[tauri::command]
#[specta::specta]
pub fn stop_handy_keys_recording(app: AppHandle) -> Result<(), String> {
    let settings = get_settings(&app);
    if settings.keyboard_implementation != settings::KeyboardImplementation::HandyKeys {
        return Err("handy-keys is not the active keyboard implementation".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.stop_recording()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn runtime_slot_drops_active_runtime_when_stopped() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut slot = RuntimeSlot::default();
        slot.install(DropProbe(Arc::clone(&drops)))
            .expect("first runtime should install");

        assert!(slot.is_running());
        let runtime = slot.take();
        assert!(runtime.is_some());
        drop(runtime);
        assert!(!slot.is_running());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn runtime_slot_is_restartable_after_stop() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut slot = RuntimeSlot::default();

        slot.install(DropProbe(Arc::clone(&drops))).unwrap();
        drop(slot.take());
        slot.install(DropProbe(Arc::clone(&drops))).unwrap();

        assert!(slot.is_running());
        drop(slot);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn runtime_slot_rejects_a_second_active_runtime() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut slot = RuntimeSlot::default();

        slot.install(DropProbe(Arc::clone(&drops))).unwrap();
        let duplicate = slot.install(DropProbe(Arc::clone(&drops)));

        assert!(duplicate.is_err());
        assert!(slot.is_running());
        drop(duplicate);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    struct LockAwareDrop {
        slot: std::sync::Weak<Mutex<RuntimeSlot<LockAwareDrop>>>,
        reacquired: Arc<AtomicBool>,
    }

    impl Drop for LockAwareDrop {
        fn drop(&mut self) {
            let reacquired = self
                .slot
                .upgrade()
                .is_some_and(|slot| slot.try_lock().is_ok());
            self.reacquired.store(reacquired, Ordering::SeqCst);
        }
    }

    #[test]
    fn runtime_is_dropped_after_controller_lock_is_released() {
        let reacquired = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(RuntimeSlot::default()));
        slot.lock()
            .unwrap()
            .install(LockAwareDrop {
                slot: Arc::downgrade(&slot),
                reacquired: Arc::clone(&reacquired),
            })
            .ok()
            .expect("runtime should install");

        let runtime = take_runtime(slot.as_ref()).unwrap();
        drop(runtime);

        assert!(reacquired.load(Ordering::SeqCst));
    }

    #[test]
    fn accessibility_revocation_stops_only_an_active_runtime() {
        assert_eq!(
            runtime_health_action(true, false),
            RuntimeHealthAction::StopAndFallback
        );
        assert_eq!(
            runtime_health_action(true, true),
            RuntimeHealthAction::Continue
        );
        assert_eq!(
            runtime_health_action(false, false),
            RuntimeHealthAction::Continue
        );
    }
}
