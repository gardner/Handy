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

use handy_keys::{
    Hotkey, HotkeyId, HotkeyManager, HotkeyState, KeyboardListener, ListenerTerminationReason,
};
use log::{debug, error, info, warn};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tauri::{AppHandle, Emitter, Manager};

use crate::settings::{self, get_settings, ShortcutBinding};

use super::handler::handle_shortcut_event;

const MANAGER_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MANAGER_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
static HANDY_KEYS_CAPTURE_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

fn register_mapped_hotkey<K: Copy + Eq + Hash, V>(
    binding_to_hotkey: &mut HashMap<String, K>,
    hotkey_to_binding: &mut HashMap<K, V>,
    binding_id: &str,
    register: impl FnOnce() -> Result<(K, V), String>,
) -> Result<bool, String> {
    if binding_to_hotkey.contains_key(binding_id) {
        return Ok(false);
    }

    let (id, value) = register()?;
    binding_to_hotkey.insert(binding_id.to_string(), id);
    hotkey_to_binding.insert(id, value);
    Ok(true)
}

fn unregister_mapped_hotkey<K: Copy + Eq + Hash, V>(
    binding_to_hotkey: &mut HashMap<String, K>,
    hotkey_to_binding: &mut HashMap<K, V>,
    binding_id: &str,
    unregister: impl FnOnce(K) -> Result<(), String>,
) -> Result<bool, String> {
    let Some(&id) = binding_to_hotkey.get(binding_id) else {
        return Ok(false);
    };

    unregister(id)?;
    binding_to_hotkey.remove(binding_id);
    hotkey_to_binding.remove(&id);
    Ok(true)
}

fn complete_capture_restore(
    capture_owner: &Mutex<Option<String>>,
    expected_owner: &str,
    restore_result: Result<(), String>,
) -> Result<(), String> {
    restore_result?;

    let mut capture_owner = capture_owner
        .lock()
        .map_err(|_| "Failed to lock recording_capture_owner")?;
    if capture_owner.as_deref() == Some(expected_owner) {
        *capture_owner = None;
    }
    Ok(())
}

/// Notify the frontend only after shortcut restoration has converged. If the
/// restore and its permission-free recovery both fail, the capture lease is
/// intentionally retained so the still-visible editor can retry cleanup.
fn finish_terminal_capture_recovery(
    restore_result: Result<(), String>,
    notify: impl FnOnce(),
) -> Result<(), String> {
    restore_result?;
    notify();
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagerCommandAcknowledgment {
    Accepted,
    TimedOut,
}

/// A command that has already timed out must not enter native code later just
/// because it was delayed in the manager queue.
fn manager_command_was_cancelled(acknowledgment: &Receiver<ManagerCommandAcknowledgment>) -> bool {
    match acknowledgment.try_recv() {
        Ok(_) | Err(mpsc::TryRecvError::Disconnected) => true,
        Err(mpsc::TryRecvError::Empty) => false,
    }
}

/// Publish a manager result and wait for the caller to explicitly accept it.
///
/// Receiver disconnection alone is not enough: `recv_timeout` can expire just
/// before its receiver is dropped, allowing a response send to succeed after
/// the caller already observed a timeout. The separate acknowledgment closes
/// that race. A successful native mutation without `Accepted` must be undone
/// before the manager processes another command.
fn manager_command_needs_rollback(
    response: Sender<Result<(), String>>,
    acknowledgment: Receiver<ManagerCommandAcknowledgment>,
    operation_result: Result<bool, String>,
) -> bool {
    let changed = matches!(operation_result, Ok(true));
    if response.send(operation_result.map(|_| ())).is_err() {
        debug!("HandyKeys command response receiver was dropped");
    }

    let accepted = matches!(
        acknowledgment.recv(),
        Ok(ManagerCommandAcknowledgment::Accepted)
    );
    changed && !accepted
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagerCommandDisposition {
    Continue,
    Restart,
}

/// Finish the response handshake and compensate a mutation the caller did not
/// accept. Even successful compensation requires a restart: it restores only
/// the state immediately before this command, which may itself be an
/// intermediate state in a multi-command transaction such as `change_binding`.
/// Rebuilding from persisted settings is the only deterministic convergence.
fn settle_manager_command(
    response: Sender<Result<(), String>>,
    acknowledgment: Receiver<ManagerCommandAcknowledgment>,
    operation_result: Result<bool, String>,
    rollback: impl FnOnce() -> Result<(), String>,
) -> ManagerCommandDisposition {
    if !manager_command_needs_rollback(response, acknowledgment, operation_result) {
        return ManagerCommandDisposition::Continue;
    }

    if let Err(error) = rollback() {
        error!("Failed to compensate an unacknowledged HandyKeys mutation: {error}");
    }
    ManagerCommandDisposition::Restart
}

fn receive_manager_response(
    response: Receiver<Result<(), String>>,
    acknowledgment: mpsc::SyncSender<ManagerCommandAcknowledgment>,
    operation: &str,
) -> Result<(), String> {
    match response.recv_timeout(MANAGER_COMMAND_TIMEOUT) {
        Ok(result) => {
            acknowledgment
                .send(ManagerCommandAcknowledgment::Accepted)
                .map_err(|_| format!("Failed to acknowledge {operation} response"))?;
            result
        }
        Err(error) => {
            // Capacity one makes this non-blocking even when the native manager
            // is still inside the operation that exceeded our deadline.
            let _ = acknowledgment.send(ManagerCommandAcknowledgment::TimedOut);
            Err(format!("Failed to receive {operation} response: {error}"))
        }
    }
}

/// Commands that can be sent to the hotkey manager thread
enum ManagerCommand {
    Register {
        binding_id: String,
        hotkey_string: String,
        response: Sender<Result<(), String>>,
        acknowledgment: Receiver<ManagerCommandAcknowledgment>,
    },
    Unregister {
        binding_id: String,
        response: Sender<Result<(), String>>,
        acknowledgment: Receiver<ManagerCommandAcknowledgment>,
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
    /// Unique shortcut-suspension lease for the current recording. This lives
    /// on the permanent controller so a failed restore remains retryable even
    /// if the native runtime is restarted or torn down.
    recording_capture_owner: Mutex<Option<String>>,
}

impl Default for HandyKeysState {
    fn default() -> Self {
        Self {
            runtime: Mutex::new(RuntimeSlot::default()),
            recording_capture_owner: Mutex::new(None),
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
    /// Per-session flag used to stop exactly the recording loop that owns it.
    /// A fresh flag for every capture prevents an old loop from being revived
    /// when another recording starts immediately after it stops.
    recording_running: Mutex<Option<Arc<AtomicBool>>>,
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
            recording_running: Mutex::new(None),
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

            if let Some(reason) = manager.termination_reason() {
                Self::dispatch_listener_termination(&app, reason);
                break;
            }

            // Check for commands (non-blocking with timeout)
            match cmd_rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(cmd) => match cmd {
                    ManagerCommand::Register {
                        binding_id,
                        hotkey_string,
                        response,
                        acknowledgment,
                    } => {
                        if manager_command_was_cancelled(&acknowledgment) {
                            debug!("Skipped timed-out HandyKeys registration for '{binding_id}'");
                            continue;
                        }
                        let result = Self::do_register(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                            &hotkey_string,
                        );
                        let disposition =
                            settle_manager_command(response, acknowledgment, result, || {
                                Self::do_unregister(
                                    &manager,
                                    &mut binding_to_hotkey,
                                    &mut hotkey_to_binding,
                                    &binding_id,
                                )
                                .map(|_| ())
                            });
                        if disposition == ManagerCommandDisposition::Restart {
                            warn!(
                                "HandyKeys registration for '{binding_id}' completed after its caller timed out; restarting from settings"
                            );
                            Self::dispatch_listener_termination(
                                &app,
                                ListenerTerminationReason::BackendStopped,
                            );
                            break;
                        }
                    }
                    ManagerCommand::Unregister {
                        binding_id,
                        response,
                        acknowledgment,
                    } => {
                        if manager_command_was_cancelled(&acknowledgment) {
                            debug!("Skipped timed-out HandyKeys unregistration for '{binding_id}'");
                            continue;
                        }
                        let previous_hotkey = binding_to_hotkey
                            .get(&binding_id)
                            .and_then(|id| hotkey_to_binding.get(id))
                            .map(|(_, hotkey)| hotkey.clone());
                        let result = Self::do_unregister(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                        );
                        let disposition = settle_manager_command(
                            response,
                            acknowledgment,
                            result,
                            || {
                                previous_hotkey
                                    .ok_or_else(|| {
                                        format!(
                                            "No prior hotkey was tracked for unregistered binding '{binding_id}'"
                                        )
                                    })
                                    .and_then(|hotkey| {
                                        Self::do_register(
                                            &manager,
                                            &mut binding_to_hotkey,
                                            &mut hotkey_to_binding,
                                            &binding_id,
                                            &hotkey,
                                        )
                                        .map(|_| ())
                                    })
                            },
                        );
                        if disposition == ManagerCommandDisposition::Restart {
                            warn!(
                                "HandyKeys unregistration for '{binding_id}' completed after its caller timed out; restarting from settings"
                            );
                            Self::dispatch_listener_termination(
                                &app,
                                ListenerTerminationReason::BackendStopped,
                            );
                            break;
                        }
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

    fn dispatch_listener_termination(app: &AppHandle, reason: ListenerTerminationReason) {
        let handy_keys_is_active = get_settings(app).keyboard_implementation
            == settings::KeyboardImplementation::HandyKeys;
        let action = listener_termination_action(
            reason,
            handy_keys_is_active,
            super::macos_accessibility_granted(),
        );

        if action == ListenerTerminationAction::Ignore {
            return;
        }

        error!("handy-keys listener terminated ({reason:?}); recovery action: {action:?}");

        // Recovery may stop this runtime and join its manager thread, so it
        // must never execute on the manager thread itself.
        let app = app.clone();
        if let Err(error) = thread::Builder::new()
            .name("handy-keys-recovery".into())
            .spawn(move || super::handle_handy_keys_termination(&app, reason))
        {
            error!("Failed to start handy-keys listener recovery: {error}");
        }
    }

    /// Register a hotkey
    fn do_register(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
        hotkey_string: &str,
    ) -> Result<bool, String> {
        let hotkey: Hotkey = hotkey_string
            .parse()
            .map_err(|e| format!("Failed to parse hotkey '{}': {}", hotkey_string, e))?;

        let registered =
            register_mapped_hotkey(binding_to_hotkey, hotkey_to_binding, binding_id, || {
                let id = manager
                    .register(hotkey)
                    .map_err(|e| format!("Failed to register hotkey: {}", e))?;
                Ok((id, (binding_id.to_string(), hotkey_string.to_string())))
            })?;
        if registered {
            debug!(
                "Registered handy-keys shortcut: {} -> {:?}",
                binding_id, hotkey
            );
        }
        Ok(registered)
    }

    /// Unregister a hotkey
    fn do_unregister(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
    ) -> Result<bool, String> {
        let unregistered =
            unregister_mapped_hotkey(binding_to_hotkey, hotkey_to_binding, binding_id, |id| {
                manager
                    .unregister(id)
                    .map_err(|e| format!("Failed to unregister hotkey: {}", e))
            })?;
        if unregistered {
            debug!("Unregistered handy-keys shortcut: {}", binding_id);
        }
        Ok(unregistered)
    }

    /// Register a shortcut binding
    pub fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let (acknowledgment_tx, acknowledgment_rx) = mpsc::sync_channel(1);
        self.command_sender
            .lock()
            .map_err(|_| "Failed to lock command_sender")?
            .send(ManagerCommand::Register {
                binding_id: binding.id.clone(),
                hotkey_string: binding.current_binding.clone(),
                response: tx,
                acknowledgment: acknowledgment_rx,
            })
            .map_err(|_| "Failed to send register command")?;

        receive_manager_response(rx, acknowledgment_tx, "register")
    }

    /// Unregister a shortcut binding
    pub fn unregister(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let (acknowledgment_tx, acknowledgment_rx) = mpsc::sync_channel(1);
        self.command_sender
            .lock()
            .map_err(|_| "Failed to lock command_sender")?
            .send(ManagerCommand::Unregister {
                binding_id: binding.id.clone(),
                response: tx,
                acknowledgment: acknowledgment_rx,
            })
            .map_err(|_| "Failed to send unregister command")?;

        receive_manager_response(rx, acknowledgment_tx, "unregister")
    }

    /// Start recording mode for a specific binding
    pub fn start_recording(
        &self,
        app: &AppHandle,
        binding_id: String,
        capture_owner: String,
    ) -> Result<(), String> {
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
        let recording_running = Arc::new(AtomicBool::new(true));
        *self
            .recording_running
            .lock()
            .map_err(|_| "Failed to lock recording_running")? =
            Some(Arc::clone(&recording_running));
        self.is_recording.store(true, Ordering::SeqCst);

        // Start a thread to emit key events to the frontend
        let app_clone = app.clone();
        thread::spawn(move || {
            HandyKeysState::recording_loop(app_clone, capture_owner, recording_running);
        });

        debug!("Started handy-keys recording mode");
        Ok(())
    }

    /// Stop recording mode
    fn stop_recording(&self) -> Result<(), String> {
        self.is_recording.store(false, Ordering::SeqCst);
        if let Some(running) = self
            .recording_running
            .lock()
            .map_err(|_| "Failed to lock recording_running")?
            .take()
        {
            running.store(false, Ordering::SeqCst);
        }

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
        // Signal recording to stop
        if let Ok(mut running) = self.recording_running.lock() {
            if let Some(running) = running.take() {
                running.store(false, Ordering::SeqCst);
            }
        }
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

        Ok(())
    }

    pub fn stop(&self) -> Result<bool, String> {
        // Take ownership while locked, then release the controller mutex before
        // Drop joins worker threads. The revocation monitor may itself be
        // waiting on this mutex to discover that another path already stopped.
        let runtime = take_runtime(&self.runtime)?;
        let stopped = runtime.is_some();
        drop(runtime);
        if let Ok(mut capture_owner) = self.recording_capture_owner.lock() {
            *capture_owner = None;
        }
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

    pub fn start_recording(
        &self,
        app: &AppHandle,
        binding_id: String,
        capture_owner: String,
    ) -> Result<(), String> {
        {
            let mut current_owner = self
                .recording_capture_owner
                .lock()
                .map_err(|_| "Failed to lock recording_capture_owner")?;
            if current_owner.is_some() {
                return Err("Already recording".into());
            }
            *current_owner = Some(capture_owner.clone());
        }

        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock handy-keys runtime")?;
        runtime
            .get()
            .ok_or_else(|| "HandyKeys runtime is not active".to_string())?
            .start_recording(app, binding_id, capture_owner)
    }

    fn stop_recording_for_owner(&self, expected_owner: &str) -> Result<bool, String> {
        let current_owner = self
            .recording_capture_owner
            .lock()
            .map_err(|_| "Failed to lock recording_capture_owner")?;
        if current_owner.as_deref() != Some(expected_owner) {
            return Ok(false);
        }

        // Keep ownership locked through the runtime mutation. Otherwise a stale
        // recovery task could validate an old lease, pause before taking the
        // runtime lock, and then stop a newer capture that reused this runtime.
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock handy-keys runtime")?;
        if let Some(runtime) = runtime.get() {
            runtime.stop_recording()?;
        }
        Ok(true)
    }

    fn recording_capture_owner(&self) -> Option<String> {
        self.recording_capture_owner.lock().ok()?.clone()
    }

    fn complete_capture_restore(
        &self,
        expected_owner: &str,
        restore_result: Result<(), String>,
    ) -> Result<(), String> {
        complete_capture_restore(
            &self.recording_capture_owner,
            expected_owner,
            restore_result,
        )
    }

    fn next_recording_update(&self, expected_owner: &str) -> RecordingListenerUpdate {
        let Ok(current_owner) = self.recording_capture_owner.lock() else {
            return RecordingListenerUpdate::Stopped;
        };
        if current_owner.as_deref() != Some(expected_owner) {
            return RecordingListenerUpdate::Stopped;
        }

        let Ok(runtime) = self.runtime.lock() else {
            return RecordingListenerUpdate::Stopped;
        };
        let Some(runtime) = runtime.get() else {
            return RecordingListenerUpdate::Stopped;
        };
        let Ok(listener) = runtime.recording_listener.lock() else {
            return RecordingListenerUpdate::Stopped;
        };
        let Some(listener) = listener.as_ref() else {
            return RecordingListenerUpdate::Stopped;
        };

        if let Some(event) = listener.try_recv() {
            RecordingListenerUpdate::Event(event)
        } else if let Some(reason) = listener.termination_reason() {
            RecordingListenerUpdate::Terminated(reason)
        } else {
            RecordingListenerUpdate::Idle
        }
    }

    /// Recording loop - emits key events to frontend during recording.
    fn recording_loop(app: AppHandle, capture_owner: String, running: Arc<AtomicBool>) {
        while running.load(Ordering::SeqCst) {
            let update = app
                .try_state::<HandyKeysState>()
                .map_or(RecordingListenerUpdate::Stopped, |state| {
                    state.next_recording_update(&capture_owner)
                });

            match update {
                RecordingListenerUpdate::Event(key_event) => {
                    if !running.load(Ordering::SeqCst)
                        || app.try_state::<HandyKeysState>().is_none_or(|state| {
                            state.recording_capture_owner().as_deref()
                                != Some(capture_owner.as_str())
                        })
                    {
                        break;
                    }
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
                }
                RecordingListenerUpdate::Idle => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                RecordingListenerUpdate::Terminated(reason) => {
                    error!("handy-keys recording listener terminated: {reason:?}");
                    running.store(false, Ordering::SeqCst);
                    let app_for_recovery = app.clone();
                    let capture_owner_for_recovery = capture_owner.clone();
                    let _ = thread::Builder::new()
                        .name("handy-keys-recording-recovery".into())
                        .spawn(move || {
                            if let Some(state) = app_for_recovery.try_state::<HandyKeysState>() {
                                match state.stop_recording_for_owner(&capture_owner_for_recovery) {
                                    Ok(true) => {
                                        let restore_result = super::resume_all_bindings(
                                            app_for_recovery.clone(),
                                            capture_owner_for_recovery.clone(),
                                        );
                                        let completion_result = state.complete_capture_restore(
                                            &capture_owner_for_recovery,
                                            restore_result,
                                        );
                                        if let Err(error) = finish_terminal_capture_recovery(
                                            completion_result,
                                            || {
                                                let _ = app_for_recovery.emit(
                                                    "handy-keys-recording-stopped",
                                                    format!("{reason:?}"),
                                                );
                                            },
                                        ) {
                                            error!("Failed to restore shortcuts after recording listener failure: {error}");
                                        }
                                    }
                                    Ok(false) => {
                                        debug!("Ignored stale recording-listener termination for {capture_owner_for_recovery}");
                                    }
                                    Err(error) => {
                                        error!("Failed to stop terminated shortcut recorder: {error}");
                                    }
                                }
                            }
                        });
                    break;
                }
                RecordingListenerUpdate::Stopped => break,
            }
        }

        debug!("Recording loop ended");
    }
}

enum RecordingListenerUpdate {
    Event(handy_keys::KeyEvent),
    Idle,
    Terminated(ListenerTerminationReason),
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListenerTerminationAction {
    Ignore,
    Fallback,
    Restart,
}

fn listener_termination_action(
    reason: ListenerTerminationReason,
    handy_keys_is_active: bool,
    accessibility_granted: bool,
) -> ListenerTerminationAction {
    if reason == ListenerTerminationReason::ShutdownRequested || !handy_keys_is_active {
        ListenerTerminationAction::Ignore
    } else if accessibility_granted {
        ListenerTerminationAction::Restart
    } else {
        ListenerTerminationAction::Fallback
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

/// End an in-progress shortcut capture before replacing the entire keyboard
/// backend. Static shortcuts are intentionally not resumed here: the caller
/// will tear down the old backend and register the replacement atomically.
pub fn cancel_recording_for_backend_teardown(
    app: &AppHandle,
    reason: &str,
) -> Result<bool, String> {
    let Some(state) = app.try_state::<HandyKeysState>() else {
        return Ok(false);
    };
    let Some(capture_owner) = state.recording_capture_owner() else {
        return Ok(false);
    };

    if !state.stop_recording_for_owner(&capture_owner)? {
        return Ok(false);
    }
    state.complete_capture_restore(&capture_owner, Ok(()))?;
    let _ = app.emit("handy-keys-recording-stopped", reason.to_string());
    Ok(true)
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

    // Secure Input suppresses key events before they reach the recorder. Refuse
    // explicitly so a partial modifier capture cannot overwrite the binding.
    if crate::secure_input::is_enabled_now() {
        crate::secure_input::note_recorder_blocked(&app);
        return Err("secure-input-active".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;

    // Suspend all active bindings so an overlapping shortcut cannot fire or
    // swallow keys while the UI captures its replacement.
    let capture_owner = format!(
        "handy_keys:{binding_id}:{}",
        HANDY_KEYS_CAPTURE_GENERATION.fetch_add(1, Ordering::SeqCst)
    );
    super::suspend_all_bindings(app.clone(), capture_owner.clone())?;
    let result = state.start_recording(&app, binding_id, capture_owner.clone());
    if let Err(start_error) = result {
        let restore_result = super::resume_all_bindings(app.clone(), capture_owner.clone());
        return match state.complete_capture_restore(&capture_owner, restore_result) {
            Ok(()) => Err(start_error),
            Err(resume_error) => Err(format!(
                "{start_error}; failed to restore shortcuts after recording startup: {resume_error}"
            )),
        };
    }
    Ok(())
}

/// Stop key recording mode
#[tauri::command]
#[specta::specta]
pub fn stop_handy_keys_recording(app: AppHandle) -> Result<(), String> {
    // Cleanup is intentionally idempotent and independent of the selected
    // backend. A failed start can race an Accessibility fallback that switches
    // settings to Tauri while retaining this controller's capture lease.
    let Some(state) = app.try_state::<HandyKeysState>() else {
        return Ok(());
    };

    let capture_owner = state.recording_capture_owner();
    let Some(capture_owner) = capture_owner else {
        return Ok(());
    };

    if !state.stop_recording_for_owner(&capture_owner)? {
        return Ok(());
    }
    let restore_result = super::resume_all_bindings(app.clone(), capture_owner.clone());
    state.complete_capture_restore(&capture_owner, restore_result)
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
    fn failed_native_unregister_preserves_both_lookup_maps_for_retry() {
        let mut binding_to_hotkey = HashMap::from([("transcribe".to_string(), 7_u32)]);
        let mut hotkey_to_binding =
            HashMap::from([(7_u32, ("transcribe".to_string(), "Alt+Space".to_string()))]);

        let result = unregister_mapped_hotkey(
            &mut binding_to_hotkey,
            &mut hotkey_to_binding,
            "transcribe",
            |_| Err("native unregister failed".to_string()),
        );

        assert_eq!(result, Err("native unregister failed".to_string()));
        assert_eq!(binding_to_hotkey.get("transcribe"), Some(&7));
        assert!(hotkey_to_binding.contains_key(&7));
    }

    #[test]
    fn failed_shortcut_restore_retains_recording_capture_owner_for_retry() {
        let capture_owner = Mutex::new(Some("handy_keys:transcribe:7".to_string()));

        let result = complete_capture_restore(
            &capture_owner,
            "handy_keys:transcribe:7",
            Err("registration failed".to_string()),
        );

        assert_eq!(result, Err("registration failed".to_string()));
        assert_eq!(
            capture_owner.lock().unwrap().as_deref(),
            Some("handy_keys:transcribe:7")
        );
    }

    #[test]
    fn terminal_recording_notification_waits_for_successful_shortcut_restore() {
        let notifications = AtomicUsize::new(0);

        let failed =
            finish_terminal_capture_recovery(Err("shortcut restore failed".to_string()), || {
                notifications.fetch_add(1, Ordering::SeqCst);
            });

        assert_eq!(failed, Err("shortcut restore failed".to_string()));
        assert_eq!(notifications.load(Ordering::SeqCst), 0);

        finish_terminal_capture_recovery(Ok(()), || {
            notifications.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn duplicate_registration_for_same_binding_is_idempotent() {
        let mut binding_to_hotkey = HashMap::from([("transcribe".to_string(), 7_u32)]);
        let mut hotkey_to_binding =
            HashMap::from([(7_u32, ("transcribe".to_string(), "Alt+Space".to_string()))]);
        let register_calls = AtomicUsize::new(0);

        let result = register_mapped_hotkey(
            &mut binding_to_hotkey,
            &mut hotkey_to_binding,
            "transcribe",
            || {
                register_calls.fetch_add(1, Ordering::SeqCst);
                Ok((8_u32, ("transcribe".to_string(), "Alt+Space".to_string())))
            },
        );

        assert_eq!(result, Ok(false));
        assert_eq!(register_calls.load(Ordering::SeqCst), 0);
        assert_eq!(binding_to_hotkey.get("transcribe"), Some(&7));
        assert!(!hotkey_to_binding.contains_key(&8));
    }

    #[test]
    fn late_manager_mutation_after_caller_timeout_requires_rollback() {
        let (response_tx, response_rx) = mpsc::channel();
        let (acknowledgment_tx, acknowledgment_rx) = mpsc::sync_channel(1);
        acknowledgment_tx
            .send(ManagerCommandAcknowledgment::TimedOut)
            .unwrap();

        let rollback_required =
            manager_command_needs_rollback(response_tx, acknowledgment_rx, Ok(true));

        assert!(rollback_required);
        assert_eq!(response_rx.recv().unwrap(), Ok(()));
    }

    #[test]
    fn compensated_late_manager_mutation_forces_settings_driven_restart() {
        let (response_tx, response_rx) = mpsc::channel();
        let (acknowledgment_tx, acknowledgment_rx) = mpsc::sync_channel(1);
        acknowledgment_tx
            .send(ManagerCommandAcknowledgment::TimedOut)
            .unwrap();
        let rollback_calls = AtomicUsize::new(0);

        let disposition = settle_manager_command(response_tx, acknowledgment_rx, Ok(true), || {
            rollback_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        assert_eq!(disposition, ManagerCommandDisposition::Restart);
        assert_eq!(rollback_calls.load(Ordering::SeqCst), 1);
        assert_eq!(response_rx.recv().unwrap(), Ok(()));
    }

    #[test]
    fn queued_manager_command_is_skipped_when_caller_already_timed_out() {
        let (acknowledgment_tx, acknowledgment_rx) = mpsc::sync_channel(1);
        acknowledgment_tx
            .send(ManagerCommandAcknowledgment::TimedOut)
            .unwrap();

        assert!(manager_command_was_cancelled(&acknowledgment_rx));
    }

    #[test]
    fn accepted_manager_mutation_does_not_require_rollback() {
        let (response_tx, response_rx) = mpsc::channel();
        let (acknowledgment_tx, acknowledgment_rx) = mpsc::sync_channel(1);
        acknowledgment_tx
            .send(ManagerCommandAcknowledgment::Accepted)
            .unwrap();

        let rollback_required =
            manager_command_needs_rollback(response_tx, acknowledgment_rx, Ok(true));

        assert!(!rollback_required);
        assert_eq!(response_rx.recv().unwrap(), Ok(()));
    }

    #[test]
    fn failed_manager_operation_never_requests_rollback() {
        let (response_tx, response_rx) = mpsc::channel();
        let (acknowledgment_tx, acknowledgment_rx) = mpsc::sync_channel(1);
        acknowledgment_tx
            .send(ManagerCommandAcknowledgment::TimedOut)
            .unwrap();

        let rollback_required = manager_command_needs_rollback(
            response_tx,
            acknowledgment_rx,
            Err("native operation failed".to_string()),
        );

        assert!(!rollback_required);
        assert_eq!(
            response_rx.recv().unwrap(),
            Err("native operation failed".to_string())
        );
    }

    #[test]
    fn terminal_listener_falls_back_when_accessibility_is_denied() {
        assert_eq!(
            listener_termination_action(
                handy_keys::ListenerTerminationReason::AccessibilityPermissionRevoked,
                true,
                false,
            ),
            ListenerTerminationAction::Fallback
        );
    }

    #[test]
    fn terminal_listener_restarts_after_permission_is_restored() {
        assert_eq!(
            listener_termination_action(
                handy_keys::ListenerTerminationReason::EventTapDisabledByUserInput,
                true,
                true,
            ),
            ListenerTerminationAction::Restart
        );
        assert_eq!(
            listener_termination_action(
                handy_keys::ListenerTerminationReason::AccessibilityPermissionRevoked,
                true,
                true,
            ),
            ListenerTerminationAction::Restart
        );
    }

    #[test]
    fn requested_or_inactive_listener_shutdown_is_ignored() {
        assert_eq!(
            listener_termination_action(
                handy_keys::ListenerTerminationReason::ShutdownRequested,
                true,
                false,
            ),
            ListenerTerminationAction::Ignore
        );
        assert_eq!(
            listener_termination_action(
                handy_keys::ListenerTerminationReason::BackendStopped,
                false,
                true,
            ),
            ListenerTerminationAction::Ignore
        );
    }
}
