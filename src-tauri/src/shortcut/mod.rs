//! Keyboard shortcut management module
//!
//! This module provides a unified interface for keyboard shortcuts with
//! multiple backend implementations:
//!
//! - `tauri`: Uses Tauri's built-in global-shortcut plugin
//! - `handy_keys`: Uses the handy-keys library for more control
//!
//! The active implementation is determined by the `keyboard_implementation`
//! setting and can be changed at runtime.

mod handler;
pub mod handy_keys;
pub mod tauri_impl;

use log::{debug, error, info, warn};
use serde::Serialize;
use specta::Type;
use tauri::{AppHandle, Emitter, Manager};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::settings::APPLE_INTELLIGENCE_DEFAULT_MODEL_ID;
use crate::settings::{
    self, get_settings, AutoSubmitKey, ClipboardHandling, KeyboardImplementation, LLMPrompt,
    OverlayPosition, OverlayStyle, PasteMethod, ShortcutBinding, SoundTheme, Theme, TypingTool,
    APPLE_INTELLIGENCE_PROVIDER_ID,
};
use crate::tray;

// Note: Commands are accessed via shortcut::handy_keys:: in lib.rs

static SHORTCUT_TRANSITION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[derive(Default)]
struct ShortcutCaptureState {
    owner: Option<String>,
}

static SHORTCUT_CAPTURE_STATE: std::sync::LazyLock<std::sync::Mutex<ShortcutCaptureState>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(ShortcutCaptureState::default()));
#[cfg(target_os = "macos")]
static GENERAL_ACCESSIBILITY_MONITOR_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(target_os = "macos")]
static GENERAL_ACCESSIBILITY_MONITOR_CONTROL: std::sync::LazyLock<(
    std::sync::Mutex<bool>,
    std::sync::Condvar,
)> = std::sync::LazyLock::new(|| (std::sync::Mutex::new(false), std::sync::Condvar::new()));
const HANDY_KEYS_RESTART_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);
static HANDY_KEYS_RESTART_GUARD: std::sync::LazyLock<std::sync::Mutex<HandyKeysRestartGuard>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HandyKeysRestartGuard::default()));

#[derive(Default)]
struct HandyKeysRestartGuard {
    last_restart: Option<std::time::Instant>,
}

impl HandyKeysRestartGuard {
    fn reserve_restart(&mut self, now: std::time::Instant) -> bool {
        if self.last_restart.is_some_and(|previous| {
            now.saturating_duration_since(previous) < HANDY_KEYS_RESTART_COOLDOWN
        }) {
            return false;
        }

        self.last_restart = Some(now);
        true
    }

    fn reset(&mut self) {
        self.last_restart = None;
    }
}

fn reserve_handy_keys_restart() -> bool {
    HANDY_KEYS_RESTART_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .reserve_restart(std::time::Instant::now())
}

fn reset_handy_keys_restart_guard() {
    HANDY_KEYS_RESTART_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .reset();
}

#[cfg(any(not(target_os = "linux"), test))]
#[derive(Clone, Copy)]
struct CancelShortcutRequest {
    generation: u64,
    desired: bool,
}

#[cfg(any(not(target_os = "linux"), test))]
struct CancelShortcutRequestTracker {
    generation: std::sync::atomic::AtomicU64,
}

#[cfg(any(not(target_os = "linux"), test))]
impl CancelShortcutRequestTracker {
    const fn new() -> Self {
        Self {
            generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn next(&self, desired: bool) -> CancelShortcutRequest {
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1);
        CancelShortcutRequest {
            generation,
            desired,
        }
    }

    fn is_current(&self, request: CancelShortcutRequest) -> bool {
        self.generation.load(std::sync::atomic::Ordering::SeqCst) == request.generation
    }
}

#[cfg(not(target_os = "linux"))]
static CANCEL_SHORTCUT_REQUESTS: CancelShortcutRequestTracker = CancelShortcutRequestTracker::new();

#[cfg(not(target_os = "linux"))]
struct RegisteredCancelShortcut {
    implementation: KeyboardImplementation,
    binding: ShortcutBinding,
}

#[cfg(not(target_os = "linux"))]
static REGISTERED_CANCEL_SHORTCUT: std::sync::Mutex<Option<RegisteredCancelShortcut>> =
    std::sync::Mutex::new(None);

fn with_shortcut_transition<T>(operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let _guard = SHORTCUT_TRANSITION_LOCK.lock().unwrap_or_else(|poisoned| {
        warn!("Shortcut transition lock was poisoned; recovering from current settings");
        poisoned.into_inner()
    });
    operation()
}

fn shortcut_capture_owner() -> std::sync::MutexGuard<'static, ShortcutCaptureState> {
    SHORTCUT_CAPTURE_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn abandon_shortcut_capture() {
    shortcut_capture_owner().owner = None;
}

fn prepare_for_backend_teardown(app: &AppHandle, reason: &str) -> Result<(), String> {
    cancel_active_recording_before_backend_teardown(app);
    handy_keys::cancel_recording_for_backend_teardown(app, reason)?;
    abandon_shortcut_capture();
    Ok(())
}

fn accessibility_fallback_needed(
    accessibility_was_granted: bool,
    configuration_requires_accessibility: bool,
    accessibility_granted: bool,
) -> bool {
    !accessibility_granted && (accessibility_was_granted || configuration_requires_accessibility)
}

fn accessibility_monitor_check_needed(configuration_requires_accessibility: bool) -> bool {
    configuration_requires_accessibility
}

/// Wake or park the macOS Accessibility monitor when the persisted feature
/// profile changes. Minimum-permissions mode performs no recurring AX calls.
#[cfg(target_os = "macos")]
pub(crate) fn set_general_accessibility_monitor_required(required: bool) {
    let (state, changed) = &*GENERAL_ACCESSIBILITY_MONITOR_CONTROL;
    let mut current = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *current != required {
        *current = required;
        changed.notify_all();
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_general_accessibility_monitor_required(_required: bool) {}

#[cfg(target_os = "macos")]
fn ensure_general_accessibility_monitor(app: &AppHandle) {
    use std::sync::atomic::Ordering;

    set_general_accessibility_monitor_required(
        settings::get_settings(app).requires_macos_accessibility(),
    );

    if GENERAL_ACCESSIBILITY_MONITOR_STARTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let app = app.clone();
    std::thread::spawn(move || {
        // Treat an initially denied grant as a revocation. Shortcut
        // initialization is serialized with the fallback, so whichever path
        // acquires the transition lock first establishes the same safe profile.
        let mut accessibility_was_granted = true;

        loop {
            let (monitor_required, changed) = &*GENERAL_ACCESSIBILITY_MONITOR_CONTROL;
            let mut required = monitor_required
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while !accessibility_monitor_check_needed(*required) {
                required = changed
                    .wait(required)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            drop(required);

            let accessibility_granted = macos_accessibility_granted();
            let configuration_requires_accessibility = *monitor_required
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            if accessibility_fallback_needed(
                accessibility_was_granted,
                configuration_requires_accessibility,
                accessibility_granted,
            ) {
                match fallback_to_minimum_permissions(&app, "permission_revoked") {
                    Ok(()) => accessibility_was_granted = false,
                    Err(error) => {
                        error!("General Accessibility fallback failed: {error}");
                        // The observed edge or restricted settings make the
                        // next denied poll retry.
                    }
                }
            } else if accessibility_granted {
                accessibility_was_granted = true;
            }
            // The native listener reports terminal event-tap failures. This
            // slower app-level check remains only as defense for silent AX
            // transitions outside the listener lifecycle.
            let required = monitor_required
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *required {
                let _ = changed.wait_timeout(required, std::time::Duration::from_millis(250));
            }
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn ensure_general_accessibility_monitor(_app: &AppHandle) {}

const ACCESSIBILITY_REQUIRED_ERROR: &str =
    "Accessibility permission is required to use this feature";

#[cfg(target_os = "macos")]
pub(crate) fn macos_accessibility_granted() -> bool {
    ::handy_keys::check_accessibility()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn macos_accessibility_granted() -> bool {
    true
}

fn validate_macos_accessibility_configuration(
    app_settings: &settings::AppSettings,
    accessibility_granted: bool,
) -> Result<(), String> {
    if app_settings.requires_macos_accessibility() && !accessibility_granted {
        Err(ACCESSIBILITY_REQUIRED_ERROR.into())
    } else {
        Ok(())
    }
}

fn validate_shortcut_accessibility(
    implementation: KeyboardImplementation,
    binding: &ShortcutBinding,
    accessibility_granted: bool,
) -> Result<(), String> {
    let requires_accessibility = implementation == KeyboardImplementation::HandyKeys
        || (implementation == KeyboardImplementation::Tauri
            && !settings::tauri_shortcut_is_permission_free_on_macos(&binding.current_binding));

    if requires_accessibility && !accessibility_granted {
        Err(ACCESSIBILITY_REQUIRED_ERROR.into())
    } else {
        Ok(())
    }
}

pub(crate) fn require_macos_accessibility(feature: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if !macos_accessibility_granted() {
            return Err(format!("{}: {}", ACCESSIBILITY_REQUIRED_ERROR, feature));
        }
    }

    #[cfg(not(target_os = "macos"))]
    let _ = feature;

    Ok(())
}

fn require_accessible_shortcut(
    implementation: KeyboardImplementation,
    binding: &ShortcutBinding,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        validate_shortcut_accessibility(implementation, binding, macos_accessibility_granted())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (implementation, binding);
        Ok(())
    }
}

fn require_synthetic_input(
    paste_method: PasteMethod,
    auto_submit: bool,
    feature: &str,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        validate_synthetic_input_accessibility(
            paste_method,
            auto_submit,
            macos_accessibility_granted(),
        )
        .map_err(|_| format!("{}: {}", ACCESSIBILITY_REQUIRED_ERROR, feature))?;
    }

    #[cfg(not(target_os = "macos"))]
    let _ = (paste_method, auto_submit, feature);

    Ok(())
}

fn validate_synthetic_input_accessibility(
    paste_method: PasteMethod,
    auto_submit: bool,
    accessibility_granted: bool,
) -> Result<(), String> {
    if paste_method.requires_synthetic_input(auto_submit) && !accessibility_granted {
        Err(ACCESSIBILITY_REQUIRED_ERROR.into())
    } else {
        Ok(())
    }
}

fn validate_keyboard_accessibility(
    app_settings: &settings::AppSettings,
    implementation: KeyboardImplementation,
    accessibility_granted: bool,
) -> Result<(), String> {
    for (id, binding) in &app_settings.bindings {
        let is_active = id == "transcribe"
            || id == "cancel"
            || (id == "transcribe_with_post_process" && app_settings.post_process_enabled);
        if is_active {
            validate_shortcut_accessibility(implementation, binding, accessibility_granted)?;
        }
    }
    Ok(())
}

fn prepare_keyboard_implementation_change(
    app_settings: &mut settings::AppSettings,
    implementation: KeyboardImplementation,
    accessibility_granted: bool,
) -> Result<Vec<String>, String> {
    app_settings.keyboard_implementation = implementation;

    // When Accessibility has already been revoked, moving away from HandyKeys
    // is an emergency downgrade: remove every remaining synthetic-input and
    // event-tap dependency in one operation.
    if implementation == KeyboardImplementation::Tauri && !accessibility_granted {
        return Ok(prepare_minimum_permissions_profile(app_settings));
    }

    validate_keyboard_accessibility(app_settings, implementation, accessibility_granted)?;
    Ok(Vec::new())
}

/// Initialize shortcuts using the configured implementation
pub fn init_shortcuts(app: &AppHandle) -> Result<(), String> {
    ensure_general_accessibility_monitor(app);
    with_shortcut_transition(|| {
        if app
            .try_state::<crate::commands::ShortcutsInitialized>()
            .is_some()
        {
            return Ok(());
        }

        init_shortcuts_unlocked(app)?;
        app.manage(crate::commands::ShortcutsInitialized);
        // Secure Input shadows are deferred until the initialized marker is
        // present, so reconcile only after the complete backend is live.
        crate::secure_input::reconcile_fallback(app);
        Ok(())
    })
}

fn init_shortcuts_unlocked(app: &AppHandle) -> Result<(), String> {
    let mut user_settings = settings::load_or_create_app_settings(app);

    #[cfg(target_os = "macos")]
    if validate_macos_accessibility_configuration(&user_settings, macos_accessibility_granted())
        .is_err()
    {
        match user_settings.keyboard_implementation {
            KeyboardImplementation::Tauri => unregister_all_tauri_shortcuts(app)?,
            KeyboardImplementation::HandyKeys => {
                shutdown_handy_keys(app)?;
            }
        }

        warn!(
            "Configured input features require Accessibility, but permission is unavailable; \
             using Tauri shortcuts with clipboard-only output"
        );
        let reset_bindings =
            settings::update_settings(app, settings::AppSettings::use_minimum_permissions_profile);
        user_settings = settings::get_settings(app);
        emit_minimum_permissions_change(app, &reset_bindings, "permission_unavailable");
        crate::secure_input::reconcile_fallback(app);
    }

    // Check which implementation to use
    match user_settings.keyboard_implementation {
        KeyboardImplementation::Tauri => {
            tauri_impl::init_shortcuts(app)?;
            Ok(())
        }
        KeyboardImplementation::HandyKeys => {
            if let Err(e) = handy_keys::init_shortcuts(app) {
                error!("Failed to initialize handy-keys shortcuts: {}", e);
                warn!("Falling back to permission-free Tauri shortcuts and manual paste");

                let reset_bindings = settings::update_settings(
                    app,
                    settings::AppSettings::use_minimum_permissions_profile,
                );
                emit_minimum_permissions_change(
                    app,
                    &reset_bindings,
                    "shortcut_initialization_failed",
                );

                crate::secure_input::reconcile_fallback(app);
                tauri_impl::init_shortcuts(app)?;
                return Ok(());
            }
            Ok(())
        }
    }
}

#[cfg(any(not(target_os = "linux"), test))]
fn register_cancel_shortcut_with<LoadSettings, AccessibilityGranted, Fallback, Register>(
    mut load_settings: LoadSettings,
    accessibility_granted: AccessibilityGranted,
    mut fallback: Fallback,
    mut register: Register,
) -> Result<(), String>
where
    LoadSettings: FnMut() -> settings::AppSettings,
    AccessibilityGranted: Fn() -> bool,
    Fallback: FnMut() -> Result<(), String>,
    Register: FnMut(KeyboardImplementation, ShortcutBinding) -> Result<(), String>,
{
    let mut current = load_settings();
    let mut granted = accessibility_granted();

    if current.requires_macos_accessibility() && !granted {
        fallback()?;
        current = load_settings();
        granted = accessibility_granted();
    }

    let binding = current
        .bindings
        .get("cancel")
        .cloned()
        .ok_or_else(|| "Cancel shortcut is missing from settings".to_string())?;
    validate_shortcut_accessibility(current.keyboard_implementation, &binding, granted)?;
    register(current.keyboard_implementation, binding)
}

#[cfg(not(target_os = "linux"))]
fn registered_cancel_shortcut() -> std::sync::MutexGuard<'static, Option<RegisteredCancelShortcut>>
{
    REGISTERED_CANCEL_SHORTCUT
        .lock()
        .unwrap_or_else(|poisoned| {
            warn!("Registered cancel-shortcut lock was poisoned; recovering");
            poisoned.into_inner()
        })
}

#[cfg(not(target_os = "linux"))]
fn cancel_shortcut_targets_match(
    registered: &RegisteredCancelShortcut,
    implementation: KeyboardImplementation,
    binding: &ShortcutBinding,
) -> bool {
    registered.implementation == implementation
        && registered.binding.id == binding.id
        && registered.binding.current_binding == binding.current_binding
}

#[cfg(not(target_os = "linux"))]
fn register_cancel_backend(
    app: &AppHandle,
    implementation: KeyboardImplementation,
    binding: ShortcutBinding,
) -> Result<(), String> {
    match implementation {
        KeyboardImplementation::Tauri => tauri_impl::register_shortcut(app, binding),
        KeyboardImplementation::HandyKeys => handy_keys::register_shortcut(app, binding),
    }
}

#[cfg(not(target_os = "linux"))]
fn unregister_cancel_backend(
    app: &AppHandle,
    registered: &RegisteredCancelShortcut,
) -> Result<(), String> {
    match registered.implementation {
        KeyboardImplementation::Tauri => {
            tauri_impl::unregister_shortcut(app, registered.binding.clone())
        }
        KeyboardImplementation::HandyKeys => {
            handy_keys::unregister_shortcut(app, registered.binding.clone())
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn reconcile_registered_cancel_shortcut(
    app: &AppHandle,
    implementation: KeyboardImplementation,
    binding: ShortcutBinding,
) -> Result<(), String> {
    let mut registered = registered_cancel_shortcut();

    if registered
        .as_ref()
        .is_some_and(|current| cancel_shortcut_targets_match(current, implementation, &binding))
    {
        return Ok(());
    }

    if let Some(current) = registered.take() {
        if let Err(error) = unregister_cancel_backend(app, &current) {
            *registered = Some(current);
            return Err(format!(
                "Failed to replace the active cancel shortcut: {error}"
            ));
        }
    }

    register_cancel_backend(app, implementation, binding.clone())?;
    *registered = Some(RegisteredCancelShortcut {
        implementation,
        binding,
    });
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn unregister_registered_cancel_shortcut(app: &AppHandle) -> Result<(), String> {
    let mut registered = registered_cancel_shortcut();
    let Some(current) = registered.take() else {
        return Ok(());
    };

    if let Err(error) = unregister_cancel_backend(app, &current) {
        *registered = Some(current);
        return Err(error);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn forget_cancel_shortcut_registration() {
    *registered_cancel_shortcut() = None;
}

#[cfg(target_os = "linux")]
fn forget_cancel_shortcut_registration() {}

fn shutdown_handy_keys(app: &AppHandle) -> Result<bool, String> {
    let stopped = handy_keys::shutdown(app)?;
    forget_cancel_shortcut_registration();
    Ok(stopped)
}

/// Register the cancel shortcut (called when recording starts)
pub fn register_cancel_shortcut(app: &AppHandle) {
    // Track the recording lifecycle independently of the active backend so a
    // sustained Secure Input episode can shadow the dynamic cancel binding.
    crate::secure_input::register_cancel_fallback(app);

    #[cfg(target_os = "linux")]
    let _ = app;

    #[cfg(not(target_os = "linux"))]
    {
        let request = CANCEL_SHORTCUT_REQUESTS.next(true);
        let app = app.clone();
        // Keep registration off the shortcut callback thread, then serialize
        // the late task with backend changes and Accessibility fallback.
        tauri::async_runtime::spawn(async move {
            let result = with_shortcut_transition(|| {
                if !CANCEL_SHORTCUT_REQUESTS.is_current(request) {
                    return Ok(());
                }

                register_cancel_shortcut_with(
                    || get_settings(&app),
                    macos_accessibility_granted,
                    || fallback_to_minimum_permissions_unlocked(&app, "permission_revoked"),
                    |implementation, binding| {
                        reconcile_registered_cancel_shortcut(&app, implementation, binding)
                    },
                )
            });

            if let Err(error) = result {
                warn!("Cancel shortcut was not registered: {error}");
            }
        });
    }
}

/// Unregister the cancel shortcut (called when recording stops)
pub fn unregister_cancel_shortcut(app: &AppHandle) {
    crate::secure_input::unregister_cancel_fallback(app);

    #[cfg(target_os = "linux")]
    let _ = app;

    #[cfg(not(target_os = "linux"))]
    {
        let request = CANCEL_SHORTCUT_REQUESTS.next(false);
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let result = with_shortcut_transition(|| {
                if !CANCEL_SHORTCUT_REQUESTS.is_current(request) {
                    return Ok(());
                }

                debug_assert!(!request.desired);
                #[cfg(target_os = "macos")]
                {
                    let current = get_settings(&app);
                    if current.requires_macos_accessibility() && !macos_accessibility_granted() {
                        fallback_to_minimum_permissions_unlocked(&app, "permission_revoked")?;
                    }
                }
                unregister_registered_cancel_shortcut(&app)
            });

            if let Err(error) = result {
                // It may already have been removed by Accessibility fallback.
                info!("Cancel shortcut was already unavailable during cleanup: {error}");
            }
        });
    }
}

/// Register a shortcut using the appropriate implementation
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let settings = get_settings(app);
    require_accessible_shortcut(settings.keyboard_implementation, &binding)?;
    match settings.keyboard_implementation {
        KeyboardImplementation::Tauri => tauri_impl::register_shortcut(app, binding),
        KeyboardImplementation::HandyKeys => handy_keys::register_shortcut(app, binding),
    }
}

/// Unregister a shortcut using the appropriate implementation
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let settings = get_settings(app);
    match settings.keyboard_implementation {
        KeyboardImplementation::Tauri => tauri_impl::unregister_shortcut(app, binding),
        KeyboardImplementation::HandyKeys => handy_keys::unregister_shortcut(app, binding),
    }
}

// ============================================================================
// Binding Management Commands
// ============================================================================

#[derive(Serialize, Type)]
pub struct BindingResponse {
    success: bool,
    binding: Option<ShortcutBinding>,
    error: Option<String>,
}

#[tauri::command]
#[specta::specta]
pub fn change_binding(
    app: AppHandle,
    id: String,
    binding: String,
) -> Result<BindingResponse, String> {
    with_shortcut_transition(|| change_binding_unlocked(app, id, binding))
}

fn change_binding_unlocked(
    app: AppHandle,
    id: String,
    binding: String,
) -> Result<BindingResponse, String> {
    // Reject empty bindings — every shortcut should have a value
    if binding.trim().is_empty() {
        return Err("Binding cannot be empty".to_string());
    }

    let settings = settings::get_settings(&app);

    // Get the binding to modify, or create it from defaults if it doesn't exist
    let binding_to_modify = match settings.bindings.get(&id) {
        Some(binding) => binding.clone(),
        None => {
            // Try to get the default binding for this id
            let default_settings = settings::get_default_settings();
            match default_settings.bindings.get(&id) {
                Some(default_binding) => {
                    warn!(
                        "Binding '{}' not found in settings, creating from defaults",
                        id
                    );
                    default_binding.clone()
                }
                None => {
                    let error_msg = format!("Binding with id '{}' not found in defaults", id);
                    warn!("change_binding error: {}", error_msg);
                    return Ok(BindingResponse {
                        success: false,
                        binding: None,
                        error: Some(error_msg),
                    });
                }
            }
        }
    };

    // Validate the new shortcut for the current keyboard implementation
    if let Err(e) = validate_shortcut_for_implementation(&binding, settings.keyboard_implementation)
    {
        warn!("change_binding validation error: {}", e);
        return Err(e);
    }

    // Create an updated binding
    let mut updated_binding = binding_to_modify.clone();
    updated_binding.current_binding = binding;
    require_accessible_shortcut(settings.keyboard_implementation, &updated_binding)?;

    #[cfg(target_os = "macos")]
    if settings.requires_macos_accessibility() && !macos_accessibility_granted() {
        fallback_to_minimum_permissions_unlocked(&app, "permission_revoked")?;
        return change_binding_unlocked(app, id, updated_binding.current_binding);
    }

    // Inactive bindings are configuration only. Registering a disabled
    // post-processing shortcut would make it live behind the UI and, for a
    // media key, create an event tap that the permission policy cannot see.
    if !binding_change_updates_runtime(&settings, &id, shortcut_capture_owner().owner.is_some()) {
        settings::update_settings(&app, |settings| {
            settings.bindings.insert(id, updated_binding.clone());
        });
        crate::secure_input::reconcile_fallback(&app);
        return Ok(BindingResponse {
            success: true,
            binding: Some(updated_binding),
            error: None,
        });
    }

    // Only unregister after the replacement has passed validation and the
    // permission gate, so a rejected setting cannot disable the old shortcut.
    if let Err(e) = unregister_shortcut(&app, binding_to_modify.clone()) {
        let error_msg = format!("Failed to unregister shortcut: {}", e);
        error!("change_binding error: {}", error_msg);
        return Ok(BindingResponse {
            success: false,
            binding: None,
            error: Some(error_msg),
        });
    }

    // Register the new binding
    if let Err(e) = register_shortcut(&app, updated_binding.clone()) {
        let error_msg = format!("Failed to register shortcut: {}", e);
        error!("change_binding error: {}", error_msg);
        if let Err(rollback_error) = register_shortcut(&app, binding_to_modify) {
            error!(
                "Failed to restore previous shortcut after registration error: {}",
                rollback_error
            );
        }
        return Ok(BindingResponse {
            success: false,
            binding: None,
            error: Some(error_msg),
        });
    }

    // Update the binding in the settings
    settings::update_settings(&app, |settings| {
        settings.bindings.insert(id, updated_binding.clone());
    });
    crate::secure_input::reconcile_fallback(&app);

    // Return the updated binding
    Ok(BindingResponse {
        success: true,
        binding: Some(updated_binding),
        error: None,
    })
}

#[tauri::command]
#[specta::specta]
pub fn reset_binding(app: AppHandle, id: String) -> Result<BindingResponse, String> {
    let binding = settings::get_stored_binding(&app, &id);
    change_binding(app, id, binding.default_binding)
}

fn binding_is_runtime_active(app_settings: &settings::AppSettings, id: &str) -> bool {
    id != "cancel" && (id != "transcribe_with_post_process" || app_settings.post_process_enabled)
}

fn binding_change_updates_runtime(
    app_settings: &settings::AppSettings,
    id: &str,
    shortcut_capture_suspended: bool,
) -> bool {
    !shortcut_capture_suspended && binding_is_runtime_active(app_settings, id)
}

/// Return the static shortcuts that should currently be registered. The
/// dynamic cancel binding belongs to the recording lifecycle.
fn active_static_bindings(app_settings: &settings::AppSettings) -> Vec<(&str, ShortcutBinding)> {
    let mut bindings: Vec<_> = app_settings
        .bindings
        .iter()
        .filter(|(id, _)| binding_is_runtime_active(app_settings, id))
        .map(|(id, binding)| (id.as_str(), binding.clone()))
        .collect();
    bindings.sort_by_key(|(id, _)| *id);
    bindings
}

/// Temporarily unregister every active static shortcut while the user records
/// a replacement. This avoids another binding firing or swallowing the input.
fn suspend_bindings_transactionally<T: Clone>(
    bindings: Vec<(&str, T)>,
    mut suspend: impl FnMut(&T) -> Result<(), String>,
    mut restore: impl FnMut(&T) -> Result<(), String>,
) -> Result<(), String> {
    let mut suspended = Vec::new();

    for (id, binding) in bindings {
        if let Err(error) = suspend(&binding) {
            let mut rollback_errors = Vec::new();
            for (restored_id, restored_binding) in suspended.iter().rev() {
                if let Err(rollback_error) = restore(restored_binding) {
                    rollback_errors.push(format!("'{restored_id}': {rollback_error}"));
                }
            }

            let mut message = format!("Could not suspend shortcut '{id}': {error}");
            if !rollback_errors.is_empty() {
                message.push_str(&format!(
                    "; failed to restore previously suspended shortcuts: {}",
                    rollback_errors.join("; ")
                ));
            }
            return Err(message);
        }
        suspended.push((id, binding));
    }

    Ok(())
}

fn suspend_all_shortcuts(app: &AppHandle) -> Result<(), String> {
    let app_settings = get_settings(app);
    suspend_bindings_transactionally(
        active_static_bindings(&app_settings),
        |binding| unregister_shortcut(app, binding.clone()),
        |binding| register_shortcut(app, binding.clone()),
    )
}

/// Re-register every active static shortcut after shortcut recording ends.
fn resume_all_shortcuts(app: &AppHandle) -> Result<(), String> {
    let mut app_settings = get_settings(app);

    #[cfg(target_os = "macos")]
    if validate_macos_accessibility_configuration(&app_settings, macos_accessibility_granted())
        .is_err()
    {
        fallback_to_minimum_permissions_unlocked(app, "permission_revoked")?;
        // Accessibility may have been restored while the fallback waited for
        // this serialized transition. Reload either the restored profile or
        // the newly applied minimum-permissions profile and converge by
        // registering its active bindings below.
        app_settings = get_settings(app);
    }

    resume_bindings_transactionally(
        active_static_bindings(&app_settings),
        |binding| register_shortcut(app, binding.clone()),
        |binding| unregister_shortcut(app, binding.clone()),
    )
}

fn recover_failed_shortcut_resume(
    resume_result: Result<(), String>,
    recover: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let Err(resume_error) = resume_result else {
        return Ok(());
    };

    recover().map_err(|recovery_error| {
        format!(
            "Could not restore suspended shortcuts ({resume_error}); permission-free recovery failed ({recovery_error})"
        )
    })
}

fn resume_bindings_transactionally<T: Clone>(
    bindings: Vec<(&str, T)>,
    mut resume: impl FnMut(&T) -> Result<(), String>,
    mut rollback: impl FnMut(&T) -> Result<(), String>,
) -> Result<(), String> {
    let mut resumed = Vec::new();

    for (id, binding) in bindings {
        if let Err(error) = resume(&binding) {
            let mut rollback_errors = Vec::new();
            for (resumed_id, resumed_binding) in resumed.iter().rev() {
                if let Err(rollback_error) = rollback(resumed_binding) {
                    rollback_errors.push(format!("'{resumed_id}': {rollback_error}"));
                }
            }

            let mut message = format!("Could not restore shortcut '{id}': {error}");
            if !rollback_errors.is_empty() {
                message.push_str(&format!(
                    "; failed to roll back partially restored shortcuts: {}",
                    rollback_errors.join("; ")
                ));
            }
            return Err(message);
        }
        resumed.push((id, binding));
    }

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn suspend_all_bindings(app: AppHandle, owner: String) -> Result<(), String> {
    with_shortcut_transition(|| {
        if shortcut_capture_owner().owner.is_some() {
            return Err("Another shortcut is already being recorded".into());
        }

        #[cfg(target_os = "macos")]
        {
            let current = get_settings(&app);
            if current.requires_macos_accessibility() && !macos_accessibility_granted() {
                fallback_to_minimum_permissions_unlocked(&app, "permission_revoked")?;
            }
        }

        suspend_all_shortcuts(&app)?;
        shortcut_capture_owner().owner = Some(owner);
        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub fn resume_all_bindings(app: AppHandle, owner: String) -> Result<(), String> {
    with_shortcut_transition(|| {
        let current_owner = shortcut_capture_owner().owner.clone();
        let Some(current_owner) = current_owner else {
            return Ok(());
        };
        if current_owner != owner {
            return Err("Shortcut recording is owned by another editor".into());
        }

        recover_failed_shortcut_resume(resume_all_shortcuts(&app), || {
            warn!("Could not restore shortcut capture cleanly; rebuilding permission-free input");
            force_minimum_permissions_unlocked(&app, "shortcut_restore_failed")
        })?;
        let mut capture = shortcut_capture_owner();
        if capture.owner.as_deref() == Some(owner.as_str()) {
            capture.owner = None;
        }
        Ok(())
    })
}

// ============================================================================
// Keyboard Implementation Switching
// ============================================================================

/// Result of changing keyboard implementation
#[derive(Serialize, Type)]
pub struct ImplementationChangeResult {
    pub success: bool,
    /// List of binding IDs that were reset to defaults due to incompatibility
    pub reset_bindings: Vec<String>,
}

#[derive(Serialize, Type)]
pub struct MinimumPermissionsResult {
    /// Bindings changed to ordinary key combinations supported by Tauri's
    /// permission-free macOS shortcut backend.
    pub reset_bindings: Vec<String>,
}

fn prepare_minimum_permissions_profile(app_settings: &mut settings::AppSettings) -> Vec<String> {
    app_settings.use_minimum_permissions_profile()
}

fn prepare_default_minimum_permissions_profile(
    app_settings: &mut settings::AppSettings,
) -> Vec<String> {
    let mut reset_bindings = app_settings.use_minimum_permissions_profile();

    for (id, default_binding) in settings::get_default_settings().bindings {
        let changed = app_settings
            .bindings
            .get(&id)
            .is_none_or(|binding| binding.current_binding != default_binding.current_binding);
        if changed {
            app_settings.bindings.insert(id.clone(), default_binding);
            reset_bindings.push(id);
        }
    }

    reset_bindings.sort();
    reset_bindings.dedup();
    reset_bindings
}

/// Persist a macOS profile that needs only microphone access.
///
/// Before post-onboarding initialization, `initialize_shortcuts` registers the
/// normalized bindings later. If shortcuts are already live, this command
/// replaces the restricted backend immediately.
#[tauri::command]
#[specta::specta]
pub fn configure_minimum_permissions(app: AppHandle) -> Result<MinimumPermissionsResult, String> {
    with_shortcut_transition(|| configure_minimum_permissions_unlocked(app))
}

fn configure_minimum_permissions_unlocked(
    app: AppHandle,
) -> Result<MinimumPermissionsResult, String> {
    let shortcuts_initialized = app
        .try_state::<crate::commands::ShortcutsInitialized>()
        .is_some();
    let previous_implementation = settings::get_settings(&app).keyboard_implementation;

    prepare_for_backend_teardown(&app, "minimum_permissions_selected")?;

    match previous_implementation {
        KeyboardImplementation::Tauri => {
            unregister_all_tauri_shortcuts(&app)?;
        }
        KeyboardImplementation::HandyKeys => {
            // Be defensive even before the initialized marker is installed:
            // a failed startup may still have created a partial runtime.
            shutdown_handy_keys(&app)?;
        }
    }

    let mut reset_bindings =
        settings::update_settings(&app, settings::AppSettings::use_minimum_permissions_profile);
    emit_minimum_permissions_change(&app, &reset_bindings, "user_selected");
    // Changing to Tauri makes existing Carbon shadows ineligible. Remove them
    // before registering the full Tauri backend to avoid duplicate shortcuts.
    crate::secure_input::reconcile_fallback(&app);

    if shortcuts_initialized {
        match register_all_shortcuts_for_implementation(&app, KeyboardImplementation::Tauri) {
            Ok(registration_resets) => reset_bindings.extend(registration_resets),
            Err(error) => {
                reset_bindings.extend(recover_permission_free_shortcuts(
                    &app,
                    &format!("Failed to activate permission-free shortcuts ({error})"),
                )?);
            }
        }
    }

    reset_bindings.sort();
    reset_bindings.dedup();

    Ok(MinimumPermissionsResult { reset_bindings })
}

fn emit_minimum_permissions_change(app: &AppHandle, reset_bindings: &[String], reason: &str) {
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "minimum_permissions_profile",
            "value": true,
            "reset_bindings": reset_bindings,
            "reason": reason,
        }),
    );
}

fn recover_permission_free_shortcuts(
    app: &AppHandle,
    primary_error: &str,
) -> Result<Vec<String>, String> {
    unregister_all_tauri_shortcuts(app).map_err(|teardown_error| {
        format!(
            "{primary_error}; failed to tear down the incomplete Tauri shortcut backend \
             ({teardown_error})"
        )
    })?;

    let reset_bindings = settings::update_settings(app, |settings| {
        prepare_default_minimum_permissions_profile(settings)
    });
    emit_minimum_permissions_change(app, &reset_bindings, "shortcut_registration_failed");
    crate::secure_input::reconcile_fallback(app);

    tauri_impl::init_shortcuts(app).map_err(|recovery_error| {
        format!(
            "{primary_error}; permission-free default shortcuts also failed ({recovery_error}). \
             Settings remain in the permission-free profile."
        )
    })?;

    Ok(reset_bindings)
}

/// Recover from a terminal native handy-keys listener without ever attempting
/// to revive a tap after Accessibility has been revoked.
pub(super) fn handle_handy_keys_termination(
    app: &AppHandle,
    reason: ::handy_keys::ListenerTerminationReason,
) {
    if reason == ::handy_keys::ListenerTerminationReason::ShutdownRequested {
        return;
    }

    let result = with_shortcut_transition(|| {
        if settings::get_settings(app).keyboard_implementation != KeyboardImplementation::HandyKeys
        {
            debug!(
                "Ignoring terminal handy-keys listener after backend change: {:?}",
                reason
            );
            return Ok(());
        }

        if !macos_accessibility_granted() {
            warn!(
                "HandyKeys listener stopped ({:?}) after Accessibility was revoked; \
                 activating permission-free input",
                reason
            );
            return fallback_to_minimum_permissions_unlocked_after_terminal_listener(
                app,
                "permission_revoked",
            );
        }

        if !reserve_handy_keys_restart() {
            warn!(
                "HandyKeys listener stopped again within {:?}; activating permission-free input",
                HANDY_KEYS_RESTART_COOLDOWN
            );
            return fallback_to_minimum_permissions_unlocked_after_terminal_listener(
                app,
                "shortcut_listener_unstable",
            );
        }

        info!(
            "HandyKeys listener stopped ({:?}) while Accessibility remains granted; restarting",
            reason
        );
        prepare_for_backend_teardown(app, "shortcut_listener_restarted")?;
        let restart_result = shutdown_handy_keys(app).and_then(|_| handy_keys::init_shortcuts(app));
        if restart_result.is_ok() && handy_keys::is_running(app) {
            crate::secure_input::reconcile_fallback(app);
            return Ok(());
        }

        let restart_error = restart_result
            .err()
            .unwrap_or_else(|| "the restarted listener was not active".to_string());
        error!("Failed to restart the terminal HandyKeys listener: {restart_error}");
        fallback_to_minimum_permissions_unlocked(app, "shortcut_listener_failed").map_err(
            |fallback_error| {
                format!(
                    "Failed to restart HandyKeys ({restart_error}); permission-free fallback also \
                     failed ({fallback_error})"
                )
            },
        )
    });

    if let Err(error) = result {
        error!("HandyKeys terminal-listener recovery failed: {error}");
    }
}

pub(crate) fn fallback_to_minimum_permissions(app: &AppHandle, reason: &str) -> Result<(), String> {
    with_shortcut_transition(|| fallback_to_minimum_permissions_unlocked(app, reason))
}

fn fallback_to_minimum_permissions_unlocked(app: &AppHandle, reason: &str) -> Result<(), String> {
    fallback_to_minimum_permissions_unlocked_with_restore_check(app, reason, true)
}

/// A terminal listener is already dead, so a quickly restored grant must not
/// cancel recovery and leave the selected backend with no working listener.
fn fallback_to_minimum_permissions_unlocked_after_terminal_listener(
    app: &AppHandle,
    reason: &str,
) -> Result<(), String> {
    fallback_to_minimum_permissions_unlocked_with_restore_check(app, reason, false)
}

fn fallback_to_minimum_permissions_unlocked_with_restore_check(
    app: &AppHandle,
    reason: &str,
    cancel_if_permission_was_restored: bool,
) -> Result<(), String> {
    fallback_to_minimum_permissions_unlocked_with_settings_update(
        app,
        reason,
        cancel_if_permission_was_restored,
        false,
        |_| {},
    )
}

fn force_minimum_permissions_unlocked(app: &AppHandle, reason: &str) -> Result<(), String> {
    fallback_to_minimum_permissions_unlocked_with_settings_update(app, reason, false, true, |_| {})
}

fn fallback_to_minimum_permissions_unlocked_with_settings_update(
    app: &AppHandle,
    reason: &str,
    cancel_if_permission_was_restored: bool,
    force: bool,
    update: impl FnOnce(&mut settings::AppSettings),
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if cancel_if_permission_was_restored
        && reason == "permission_revoked"
        && macos_accessibility_granted()
    {
        // The transition may have waited behind another shortcut operation
        // while the user restored the grant in System Settings.
        return Ok(());
    }

    let current = settings::get_settings(app);
    if !force && !current.requires_macos_accessibility() {
        return Ok(());
    }

    let initialized = app
        .try_state::<crate::commands::ShortcutsInitialized>()
        .is_some();

    // Revocation can swallow the key-up that normally stops push-to-talk and
    // also removes the dynamic Escape binding. Cancel before tearing down the
    // old backend so recording and coordinator state cannot remain wedged.
    prepare_for_backend_teardown(app, reason)?;

    match current.keyboard_implementation {
        KeyboardImplementation::HandyKeys => {
            shutdown_handy_keys(app)?;
        }
        KeyboardImplementation::Tauri => {
            unregister_all_tauri_shortcuts(app)?;
        }
    }

    let reset_bindings = settings::update_settings(app, |settings| {
        update(settings);
        settings.use_minimum_permissions_profile()
    });
    emit_minimum_permissions_change(app, &reset_bindings, reason);
    if reason == "permission_revoked" {
        let _ = app.emit("macos-accessibility-revoked", ());
    }
    crate::secure_input::reconcile_fallback(app);

    if initialized {
        if let Err(error) = tauri_impl::init_shortcuts(app) {
            recover_permission_free_shortcuts(
                app,
                &format!("Failed to activate permission-free shortcuts ({error})"),
            )?;
        }
    }
    Ok(())
}

/// Change the keyboard implementation with runtime switching.
/// This will unregister all shortcuts from the old implementation,
/// validate shortcuts for the new implementation (resetting invalid ones to defaults),
/// and register them with the new implementation.
#[tauri::command]
#[specta::specta]
pub fn change_keyboard_implementation_setting(
    app: AppHandle,
    implementation: String,
) -> Result<ImplementationChangeResult, String> {
    with_shortcut_transition(|| {
        change_keyboard_implementation_setting_unlocked(app, implementation)
    })
}

fn change_keyboard_implementation_setting_unlocked(
    app: AppHandle,
    implementation: String,
) -> Result<ImplementationChangeResult, String> {
    let current_settings = settings::get_settings(&app);
    let current_impl = current_settings.keyboard_implementation;
    let new_impl = parse_keyboard_implementation(&implementation);

    // If same implementation, nothing to do
    if current_impl == new_impl {
        if new_impl == KeyboardImplementation::HandyKeys {
            if let Err(error) = require_macos_accessibility("advanced keyboard shortcuts") {
                fallback_to_minimum_permissions_unlocked(&app, "permission_revoked")?;
                return Err(error);
            }
            if !handy_keys::is_running(&app) {
                if let Err(error) = handy_keys::init_shortcuts(&app) {
                    fallback_to_minimum_permissions_unlocked(
                        &app,
                        "shortcut_initialization_failed",
                    )?;
                    return Err(format!(
                        "Failed to restart HandyKeys: {error}. Reverted to permission-free input."
                    ));
                }
            }
            crate::secure_input::reconcile_fallback(&app);
        }
        return Ok(ImplementationChangeResult {
            success: true,
            reset_bindings: vec![],
        });
    }

    info!(
        "Switching keyboard implementation from {:?} to {:?}",
        current_impl, new_impl
    );

    let accessibility_granted = macos_accessibility_granted();
    let mut updated_settings = current_settings.clone();
    let mut reset_bindings = prepare_keyboard_implementation_change(
        &mut updated_settings,
        new_impl,
        accessibility_granted,
    )?;

    // Validate the requested target before disturbing a working backend or an
    // active shortcut editor. Revoked HandyKeys -> Tauri remains valid and is
    // torn down immediately after this pure validation step.
    prepare_for_backend_teardown(&app, "keyboard_backend_changed")?;

    // Destroy HandyKeys itself rather than merely unregistering its bindings.
    // Dropping the runtime tears down the underlying native event tap.
    match current_impl {
        KeyboardImplementation::Tauri => unregister_all_tauri_shortcuts(&app)?,
        KeyboardImplementation::HandyKeys => {
            shutdown_handy_keys(&app)?;
        }
    }

    if new_impl == KeyboardImplementation::HandyKeys {
        reset_handy_keys_restart_guard();
        // Publish the gated backend before starting the revocation monitor.
        // If permission disappears immediately, the monitor's safe fallback is
        // then the final settings write rather than being overwritten here.
        settings::update_settings(&app, |settings| {
            settings.keyboard_implementation = KeyboardImplementation::HandyKeys;
        });
        if let Err(error) = handy_keys::init_shortcuts(&app) {
            error!("Failed to initialize HandyKeys: {}", error);
            let reset_bindings = settings::update_settings(
                &app,
                settings::AppSettings::use_minimum_permissions_profile,
            );
            emit_minimum_permissions_change(
                &app,
                &reset_bindings,
                "shortcut_initialization_failed",
            );
            crate::secure_input::reconcile_fallback(&app);
            if let Err(fallback_error) = tauri_impl::init_shortcuts(&app) {
                recover_permission_free_shortcuts(
                    &app,
                    &format!(
                        "HandyKeys initialization failed ({error}); initial permission-free \
                         fallback failed ({fallback_error})"
                    ),
                )?;
            }
            emit_keyboard_implementation_change(&app, "tauri", &reset_bindings);
            return Err(format!(
                "Failed to initialize HandyKeys: {}. Reverted to permission-free input.",
                error
            ));
        }
        if !handy_keys::is_running(&app) {
            fallback_to_minimum_permissions_unlocked(&app, "shortcut_initialization_failed")?;
            return Err(
                "Accessibility was revoked while HandyKeys initialized; using permission-free input"
                    .into(),
            );
        }

        crate::secure_input::reconcile_fallback(&app);
        emit_keyboard_implementation_change(&app, &implementation, &[]);
        return Ok(ImplementationChangeResult {
            success: true,
            reset_bindings: vec![],
        });
    }

    if new_impl == KeyboardImplementation::Tauri && !accessibility_granted {
        reset_bindings.extend(settings::update_settings(
            &app,
            settings::AppSettings::use_minimum_permissions_profile,
        ));
    } else {
        settings::update_settings(&app, |settings| {
            settings.keyboard_implementation = new_impl;
        });
    }

    if new_impl == KeyboardImplementation::Tauri {
        // Existing Secure Input shadows also use Tauri's plugin. Remove them
        // after publishing the backend change and before full registration.
        crate::secure_input::reconcile_fallback(&app);
    }

    match register_all_shortcuts_for_implementation(&app, new_impl) {
        Ok(registration_resets) => reset_bindings.extend(registration_resets),
        Err(registration_error) => {
            if new_impl == KeyboardImplementation::Tauri {
                unregister_all_tauri_shortcuts(&app).map_err(|teardown_error| {
                    format!(
                        "Failed to initialize Tauri shortcuts ({registration_error}) and could \
                         not fully tear down the partial backend ({teardown_error})"
                    )
                })?;
            }

            // A normal backend switch should restore the backend that was
            // working before the request whenever Accessibility still allows
            // it. If that is no longer possible, retain a deterministic
            // permission-free profile and retry only default shortcuts.
            if current_impl == KeyboardImplementation::HandyKeys && macos_accessibility_granted() {
                settings::update_settings(&app, |settings| {
                    settings.keyboard_implementation = KeyboardImplementation::HandyKeys;
                });
                if handy_keys::init_shortcuts(&app).is_ok() {
                    crate::secure_input::reconcile_fallback(&app);
                    emit_keyboard_implementation_change(&app, "handy_keys", &[]);
                    return Err(format!(
                        "Failed to initialize Tauri shortcuts: {registration_error}. Restored \
                         HandyKeys."
                    ));
                }
            }

            recover_permission_free_shortcuts(
                &app,
                &format!(
                    "Failed to initialize the requested shortcut backend ({registration_error})"
                ),
            )?;
            return Err(format!(
                "Failed to initialize the requested shortcut backend: {registration_error}. \
                 Reverted to permission-free default shortcuts."
            ));
        }
    }
    reset_bindings.sort();
    reset_bindings.dedup();

    emit_keyboard_implementation_change(&app, &implementation, &reset_bindings);

    info!("Keyboard implementation switched to {:?}", new_impl);

    Ok(ImplementationChangeResult {
        success: true,
        reset_bindings,
    })
}

fn cancel_active_recording_before_backend_teardown(app: &AppHandle) {
    if app
        .try_state::<std::sync::Arc<crate::managers::audio::AudioRecordingManager>>()
        .is_some_and(|manager| manager.is_recording())
    {
        crate::utils::cancel_current_operation(app);
    }
}

fn emit_keyboard_implementation_change(
    app: &AppHandle,
    implementation: &str,
    reset_bindings: &[String],
) {
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "keyboard_implementation",
            "value": implementation,
            "reset_bindings": reset_bindings
        }),
    );
}

/// Get the current keyboard implementation
#[tauri::command]
#[specta::specta]
pub fn get_keyboard_implementation(app: AppHandle) -> String {
    let settings = settings::get_settings(&app);
    match settings.keyboard_implementation {
        KeyboardImplementation::Tauri => "tauri".to_string(),
        KeyboardImplementation::HandyKeys => "handy_keys".to_string(),
    }
}

// ============================================================================
// Validation Helpers
// ============================================================================

/// Validate a shortcut for a specific implementation
fn validate_shortcut_for_implementation(
    raw: &str,
    implementation: KeyboardImplementation,
) -> Result<(), String> {
    match implementation {
        KeyboardImplementation::Tauri => tauri_impl::validate_shortcut(raw),
        KeyboardImplementation::HandyKeys => handy_keys::validate_shortcut(raw),
    }
}

/// Parse a keyboard implementation string into the enum
fn parse_keyboard_implementation(s: &str) -> KeyboardImplementation {
    match s {
        "tauri" => KeyboardImplementation::Tauri,
        "handy_keys" => KeyboardImplementation::HandyKeys,
        other => {
            warn!(
                "Invalid keyboard implementation '{}', defaulting to tauri",
                other
            );
            KeyboardImplementation::Tauri
        }
    }
}

/// Unregister every shortcut owned by Tauri's backend.
///
/// Teardown fails closed: callers must not persist or announce a permission-free
/// profile while an old media-key event tap may still be active. A backend-wide
/// teardown also catches stale registrations that no longer match settings.
fn unregister_all_tauri_shortcuts(app: &AppHandle) -> Result<(), String> {
    tauri_impl::unregister_all_shortcuts(app)?;
    forget_cancel_shortcut_registration();
    Ok(())
}

/// Register all shortcuts for a specific implementation, validating and resetting invalid ones
fn register_all_shortcuts_for_implementation(
    app: &AppHandle,
    implementation: KeyboardImplementation,
) -> Result<Vec<String>, String> {
    let mut reset_bindings = Vec::new();
    let mut reset_values = Vec::new();
    let mut registered_bindings = Vec::new();
    let default_bindings = settings::get_default_settings().bindings;
    let current_settings = settings::get_settings(app);

    for (id, default_binding) in &default_bindings {
        // Skip cancel shortcut as it's dynamically registered
        if id == "cancel" {
            continue;
        }

        // Skip post-processing shortcut when the feature is disabled
        if id == "transcribe_with_post_process" && !current_settings.post_process_enabled {
            continue;
        }

        let mut binding = current_settings
            .bindings
            .get(id)
            .cloned()
            .unwrap_or_else(|| default_binding.clone());

        // Validate the shortcut for the target implementation
        if let Err(e) =
            validate_shortcut_for_implementation(&binding.current_binding, implementation)
        {
            info!(
                "Shortcut '{}' ({}) is invalid for {:?}: {}. Resetting to default.",
                id, binding.current_binding, implementation, e
            );

            // Reset to default
            binding.current_binding = default_binding.current_binding.clone();
            reset_values.push((id.clone(), binding.clone()));
            reset_bindings.push(id.clone());
        }

        // Register with the appropriate implementation
        let result = match implementation {
            KeyboardImplementation::Tauri => tauri_impl::register_shortcut(app, binding.clone()),
            KeyboardImplementation::HandyKeys => {
                handy_keys::register_shortcut(app, binding.clone())
            }
        };

        if let Err(error) = result {
            for registered_binding in registered_bindings {
                let _ = match implementation {
                    KeyboardImplementation::Tauri => {
                        tauri_impl::unregister_shortcut(app, registered_binding)
                    }
                    KeyboardImplementation::HandyKeys => {
                        handy_keys::unregister_shortcut(app, registered_binding)
                    }
                };
            }
            return Err(format!(
                "Failed to register shortcut '{}' for {:?}: {}",
                id, implementation, error
            ));
        }
        registered_bindings.push(binding);
    }

    // Save settings if any bindings were reset
    if !reset_values.is_empty() {
        settings::update_settings(app, |settings| {
            for (id, binding) in reset_values {
                settings.bindings.insert(id, binding);
            }
        });
    }

    Ok(reset_bindings)
}

// ============================================================================
// General Settings Commands
// ============================================================================

#[tauri::command]
#[specta::specta]
pub fn change_ptt_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.push_to_talk = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_audio_feedback_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.audio_feedback = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_audio_feedback_volume_setting(app: AppHandle, volume: f32) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.audio_feedback_volume = volume;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_sound_theme_setting(app: AppHandle, theme: String) -> Result<(), String> {
    let parsed = match theme.as_str() {
        "marimba" => SoundTheme::Marimba,
        "pop" => SoundTheme::Pop,
        "custom" => SoundTheme::Custom,
        other => {
            warn!("Invalid sound theme '{}', defaulting to marimba", other);
            SoundTheme::Marimba
        }
    };
    settings::update_settings(&app, |settings| {
        settings.sound_theme = parsed;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_theme_setting(app: AppHandle, theme: String) -> Result<(), String> {
    let parsed = match theme.as_str() {
        "system" => Theme::System,
        "light" => Theme::Light,
        "dark" => Theme::Dark,
        other => {
            warn!("Invalid theme '{}', defaulting to system", other);
            Theme::System
        }
    };
    settings::update_settings(&app, |settings| {
        settings.theme = parsed;
    });
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    apply_window_theme(&app, parsed);
    // Other webviews (notably the recording overlay) own their document theme
    // and need an explicit live update.
    let _ = app.emit("theme-changed", parsed);
    Ok(())
}

/// Applies the appearance setting to native window chrome. On macOS this is
/// app-wide (`NSApp.appearance`); on Windows it updates the title bar.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn apply_window_theme(app: &AppHandle, theme: Theme) {
    let window_theme = match theme {
        Theme::System => None,
        Theme::Light => Some(tauri::Theme::Light),
        Theme::Dark => Some(tauri::Theme::Dark),
    };
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.set_theme(window_theme) {
            warn!("Failed to apply window theme: {}", e);
        }
    }
}

#[tauri::command]
#[specta::specta]
pub fn change_translate_to_english_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.translate_to_english = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_selected_language_setting(app: AppHandle, language: String) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.selected_language = language;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_overlay_position_setting(app: AppHandle, position: String) -> Result<(), String> {
    let parsed = match position.as_str() {
        // "none" is retired (visibility is overlay_style now); fold legacy callers
        // onto Bottom rather than warn.
        "none" | "bottom" => OverlayPosition::Bottom,
        "top" => OverlayPosition::Top,
        other => {
            warn!("Invalid overlay position '{}', defaulting to bottom", other);
            OverlayPosition::Bottom
        }
    };
    settings::update_settings(&app, |settings| {
        settings.overlay_position = parsed;
    });

    // Whether the overlay shows at all is owned by overlay_style now; position
    // only ever toggles Top/Bottom, so the enabled cache is untouched here.
    // Update overlay position without recreating window
    crate::utils::update_overlay_position(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_overlay_style_setting(app: AppHandle, style: String) -> Result<(), String> {
    let parsed = match style.as_str() {
        "none" => OverlayStyle::None,
        "minimal" => OverlayStyle::Minimal,
        "live" => OverlayStyle::Live,
        other => {
            warn!("Invalid overlay style '{}', defaulting to minimal", other);
            OverlayStyle::Minimal
        }
    };
    settings::update_settings(&app, |settings| {
        settings.overlay_style = parsed;
    });

    // Keep the cached overlay-enabled flag in sync so emit_levels stops (or
    // resumes) emitting on the next audio callback.
    crate::overlay::update_overlay_enabled_cache(parsed != OverlayStyle::None);

    // Reposition in case the window needs to re-center for the new style.
    crate::utils::update_overlay_position(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_debug_mode_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.debug_mode = enabled;
    });

    // Keep webview log streaming in sync: the live log viewer only exists in
    // debug mode, so logs are forwarded to the frontend only while it is on.
    crate::WEBVIEW_LOG_STREAMING.store(enabled, std::sync::atomic::Ordering::Relaxed);

    // Emit event to notify frontend of debug mode change
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "debug_mode",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_start_hidden_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.start_hidden = enabled;
    });

    // Notify frontend
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "start_hidden",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_autostart_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.autostart_enabled = enabled;
    });

    // Apply the autostart setting immediately using the platform-specific
    // helper (SMAppService on supported macOS versions).
    crate::autostart::apply_autostart(&app, enabled);

    // Notify frontend
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "autostart_enabled",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_update_checks_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.update_checks_enabled = enabled;
    });

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "update_checks_enabled",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_show_whats_new_on_update_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.show_whats_new_on_update = enabled;
    });

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "show_whats_new_on_update",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_whats_new_last_seen_version_setting(
    app: AppHandle,
    version: String,
) -> Result<(), String> {
    let version = version.trim().to_string();
    settings::update_settings(&app, |settings| {
        settings.whats_new_last_seen_version = version.clone();
    });

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "whats_new_last_seen_version",
            "value": version
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn update_custom_words(app: AppHandle, words: Vec<String>) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.custom_words = words;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_word_correction_threshold_setting(
    app: AppHandle,
    threshold: f64,
) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.word_correction_threshold = threshold;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_extra_recording_buffer_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.extra_recording_buffer_ms = ms;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_paste_delay_ms_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.paste_delay_ms = ms;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_paste_delay_after_ms_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.paste_delay_after_ms = ms;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_reliable_paste_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.reliable_paste = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_paste_method_setting(app: AppHandle, method: String) -> Result<(), String> {
    with_shortcut_transition(|| {
        let current = settings::get_settings(&app);
        let parsed = match method.as_str() {
            "ctrl_v" => PasteMethod::CtrlV,
            "direct" => PasteMethod::Direct,
            "none" => PasteMethod::None,
            "shift_insert" => PasteMethod::ShiftInsert,
            "ctrl_shift_v" => PasteMethod::CtrlShiftV,
            "external_script" => PasteMethod::ExternalScript,
            other => {
                warn!("Invalid paste method '{}', defaulting to ctrl_v", other);
                PasteMethod::CtrlV
            }
        };
        require_synthetic_input(parsed, current.auto_submit, "automatic text input")?;
        if parsed.requires_synthetic_input(current.auto_submit) {
            crate::commands::initialize_enigo(app.clone())?;
        }
        settings::update_settings(&app, |settings| {
            settings.paste_method = parsed;
        });
        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub fn get_available_typing_tools() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        crate::clipboard::get_available_typing_tools()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec!["auto".to_string()]
    }
}

#[tauri::command]
#[specta::specta]
pub fn change_typing_tool_setting(app: AppHandle, tool: String) -> Result<(), String> {
    let parsed = match tool.as_str() {
        "auto" => TypingTool::Auto,
        "wtype" => TypingTool::Wtype,
        "kwtype" => TypingTool::Kwtype,
        "dotool" => TypingTool::Dotool,
        "ydotool" => TypingTool::Ydotool,
        "xdotool" => TypingTool::Xdotool,
        other => {
            warn!("Invalid typing tool '{}', defaulting to auto", other);
            TypingTool::Auto
        }
    };
    settings::update_settings(&app, |settings| {
        settings.typing_tool = parsed;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_external_script_path_setting(
    app: AppHandle,
    path: Option<String>,
) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.external_script_path = path;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_clipboard_handling_setting(app: AppHandle, handling: String) -> Result<(), String> {
    let parsed = match handling.as_str() {
        "dont_modify" => ClipboardHandling::DontModify,
        "copy_to_clipboard" => ClipboardHandling::CopyToClipboard,
        other => {
            warn!(
                "Invalid clipboard handling '{}', defaulting to dont_modify",
                other
            );
            ClipboardHandling::DontModify
        }
    };
    settings::update_settings(&app, |settings| {
        settings.clipboard_handling = parsed;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_auto_submit_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    with_shortcut_transition(|| {
        let current = settings::get_settings(&app);
        if enabled {
            require_synthetic_input(current.paste_method, true, "automatic submission")?;
            if current.paste_method.requires_synthetic_input(true) {
                crate::commands::initialize_enigo(app.clone())?;
            }
        }
        settings::update_settings(&app, |settings| {
            settings.auto_submit = enabled;
        });
        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub fn change_auto_submit_key_setting(app: AppHandle, key: String) -> Result<(), String> {
    let parsed = match key.as_str() {
        "enter" => AutoSubmitKey::Enter,
        "ctrl_enter" => AutoSubmitKey::CtrlEnter,
        "cmd_enter" => AutoSubmitKey::CmdEnter,
        other => {
            warn!("Invalid auto submit key '{}', defaulting to enter", other);
            AutoSubmitKey::Enter
        }
    };
    settings::update_settings(&app, |settings| {
        settings.auto_submit_key = parsed;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    with_shortcut_transition(|| {
        let current = settings::get_settings(&app);
        let binding = current
            .bindings
            .get("transcribe_with_post_process")
            .cloned();

        #[cfg(target_os = "macos")]
        if current.requires_macos_accessibility() && !macos_accessibility_granted() {
            fallback_to_minimum_permissions_unlocked_with_settings_update(
                &app,
                "permission_revoked",
                true,
                false,
                |settings| {
                    if !enabled {
                        settings.post_process_enabled = false;
                    }
                },
            )?;
            if !enabled {
                // If fallback applied, it persisted the disabled state before
                // re-registering the permission-free backend. If permission
                // was restored while this transition waited, continue below
                // and perform the ordinary per-binding unregister instead.
                if !settings::get_settings(&app).post_process_enabled {
                    return Ok(());
                }
            } else {
                return Err(ACCESSIBILITY_REQUIRED_ERROR.into());
            }
        }

        if enabled {
            if let Some(binding) = binding.as_ref() {
                require_accessible_shortcut(current.keyboard_implementation, binding)?;
            }
        }

        // Apply the runtime change before persistence so a failed registration
        // cannot leave an inaccessible feature selected in settings.
        if let Some(binding) = binding {
            if enabled {
                register_shortcut(&app, binding)?;
            } else {
                unregister_shortcut(&app, binding).map_err(|error| {
                    format!("Failed to unregister disabled post-process shortcut: {error}")
                })?;
            }
        }

        settings::update_settings(&app, |settings| {
            settings.post_process_enabled = enabled;
        });
        crate::secure_input::reconcile_fallback(&app);
        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub fn change_experimental_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.experimental_enabled = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_base_url_setting(
    app: AppHandle,
    provider_id: String,
    base_url: String,
) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        let provider = settings
            .post_process_provider_mut(&provider_id)
            .ok_or_else(|| format!("Provider '{}' not found", provider_id))?;

        if provider.id != "custom" {
            return Err(format!(
                "Provider '{}' does not allow editing the base URL",
                provider.label
            ));
        }

        provider.base_url = base_url;
        Ok(())
    })
}

/// Generic helper to validate provider exists
fn validate_provider_exists(
    settings: &settings::AppSettings,
    provider_id: &str,
) -> Result<(), String> {
    if !settings
        .post_process_providers
        .iter()
        .any(|provider| provider.id == provider_id)
    {
        return Err(format!("Provider '{}' not found", provider_id));
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_api_key_setting(
    app: AppHandle,
    provider_id: String,
    api_key: String,
) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        validate_provider_exists(settings, &provider_id)?;
        settings.post_process_api_keys.insert(provider_id, api_key);
        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_model_setting(
    app: AppHandle,
    provider_id: String,
    model: String,
) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        validate_provider_exists(settings, &provider_id)?;
        settings.post_process_models.insert(provider_id, model);
        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub fn set_post_process_provider(app: AppHandle, provider_id: String) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        validate_provider_exists(settings, &provider_id)?;
        settings.post_process_provider_id = provider_id;
        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub fn add_post_process_prompt(
    app: AppHandle,
    name: String,
    prompt: String,
) -> Result<LLMPrompt, String> {
    // Generate unique ID using timestamp and random component
    let id = format!("prompt_{}", chrono::Utc::now().timestamp_millis());

    let new_prompt = LLMPrompt {
        id: id.clone(),
        name,
        prompt,
    };

    settings::update_settings(&app, |settings| {
        settings.post_process_prompts.push(new_prompt.clone());
    });

    Ok(new_prompt)
}

#[tauri::command]
#[specta::specta]
pub fn update_post_process_prompt(
    app: AppHandle,
    id: String,
    name: String,
    prompt: String,
) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        let existing_prompt = settings
            .post_process_prompts
            .iter_mut()
            .find(|prompt| prompt.id == id)
            .ok_or_else(|| format!("Prompt with id '{}' not found", id))?;
        existing_prompt.name = name;
        existing_prompt.prompt = prompt;
        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub fn delete_post_process_prompt(app: AppHandle, id: String) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        // Don't allow deleting the last prompt
        if settings.post_process_prompts.len() <= 1 {
            return Err("Cannot delete the last prompt".to_string());
        }

        // Find and remove the prompt
        let original_len = settings.post_process_prompts.len();
        settings
            .post_process_prompts
            .retain(|prompt| prompt.id != id);

        if settings.post_process_prompts.len() == original_len {
            return Err(format!("Prompt with id '{}' not found", id));
        }

        // If the deleted prompt was selected, select the first one or None
        if settings.post_process_selected_prompt_id.as_ref() == Some(&id) {
            settings.post_process_selected_prompt_id = settings
                .post_process_prompts
                .first()
                .map(|prompt| prompt.id.clone());
        }

        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub async fn fetch_post_process_models(
    app: AppHandle,
    provider_id: String,
) -> Result<Vec<String>, String> {
    let settings = settings::get_settings(&app);

    // Find the provider
    let provider = settings
        .post_process_provider(&provider_id)
        .ok_or_else(|| format!("Provider '{}' not found", provider_id))?;

    if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            return Ok(vec![APPLE_INTELLIGENCE_DEFAULT_MODEL_ID.to_string()]);
        }

        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            return Err("Apple Intelligence is only available on Apple silicon Macs running macOS 15 or later.".to_string());
        }
    }

    // Get API key
    let api_key = settings
        .post_process_api_keys
        .get(&provider_id)
        .cloned()
        .unwrap_or_default();

    // Skip fetching if no API key for providers that typically need one
    if api_key.trim().is_empty() && provider.id != "custom" {
        return Err(format!(
            "API key is required for {}. Please add an API key to list available models.",
            provider.label
        ));
    }

    crate::llm_client::fetch_models(provider, api_key).await
}

#[tauri::command]
#[specta::specta]
pub fn set_post_process_selected_prompt(app: AppHandle, id: String) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        // Verify the prompt exists
        if !settings
            .post_process_prompts
            .iter()
            .any(|prompt| prompt.id == id)
        {
            return Err(format!("Prompt with id '{}' not found", id));
        }

        settings.post_process_selected_prompt_id = Some(id);
        Ok(())
    })
}

#[tauri::command]
#[specta::specta]
pub fn change_mute_while_recording_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.mute_while_recording = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_append_trailing_space_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.append_trailing_space = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_lazy_stream_close_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.lazy_stream_close = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_vad_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.vad_enabled = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_filler_word_removal_enabled_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.filler_word_removal_enabled = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_app_language_setting(app: AppHandle, language: String) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.app_language = language.clone();
    });

    // Refresh the tray menu with the new language
    tray::update_tray_menu(&app, Some(&language));

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_show_tray_icon_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    settings::update_settings(&app, |settings| {
        settings.show_tray_icon = enabled;
    });

    // Apply change immediately
    tray::set_tray_visibility(&app, enabled);

    Ok(())
}

/// Save accelerator settings and make the next model use reload with them.
/// The currently running transcription, if any, keeps its existing engine.
fn save_accelerator_and_reload_next_use(
    app: &AppHandle,
    update: impl FnOnce(&mut settings::AppSettings),
) {
    settings::update_settings(app, update);

    let tm = app.state::<std::sync::Arc<crate::managers::transcription::TranscriptionManager>>();
    tm.reload_model_on_next_use();
}

#[tauri::command]
#[specta::specta]
pub fn change_transcribe_accelerator_setting(
    app: AppHandle,
    accelerator: settings::TranscribeAcceleratorSetting,
) -> Result<(), String> {
    save_accelerator_and_reload_next_use(&app, |settings| {
        settings.transcribe_accelerator = accelerator;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_ort_accelerator_setting(
    app: AppHandle,
    accelerator: settings::OrtAcceleratorSetting,
) -> Result<(), String> {
    save_accelerator_and_reload_next_use(&app, |settings| {
        settings.ort_accelerator = accelerator;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_transcribe_gpu_device(app: AppHandle, device: i32) -> Result<(), String> {
    save_accelerator_and_reload_next_use(&app, |settings| {
        settings.transcribe_gpu_device = device;
    });
    Ok(())
}

/// Return which accelerators and GPU devices are available for this build.
///
/// First-call cost is dominated by enumerating GPU devices through the
/// transcribe.cpp Metal/Vulkan backend, which loads dynamic libraries and
/// probes hardware. Run it on the blocking pool so the webview thread
/// stays responsive — see also the startup pre-warm in `lib.rs`.
#[tauri::command]
#[specta::specta]
pub async fn get_available_accelerators() -> crate::managers::transcription::AvailableAccelerators {
    tauri::async_runtime::spawn_blocking(crate::managers::transcription::get_available_accelerators)
        .await
        .expect("get_available_accelerators panicked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_transitions_are_serialized() {
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let overlap = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();

        for _ in 0..8 {
            let active = std::sync::Arc::clone(&active);
            let overlap = std::sync::Arc::clone(&overlap);
            let barrier = std::sync::Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                with_shortcut_transition(|| {
                    if active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) != 0 {
                        overlap.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            }));
        }

        for thread in threads {
            thread.join().unwrap();
        }
        assert!(!overlap.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn denied_permission_rechecks_restricted_configuration_without_observed_grant_edge() {
        assert!(accessibility_fallback_needed(false, true, false));
        assert!(accessibility_fallback_needed(true, false, false));
        assert!(!accessibility_fallback_needed(false, false, false));
        assert!(!accessibility_fallback_needed(true, true, true));
    }

    #[test]
    fn permission_free_profile_does_not_poll_accessibility() {
        assert!(!accessibility_monitor_check_needed(false));
        assert!(accessibility_monitor_check_needed(true));
    }

    #[test]
    fn repeated_listener_failure_falls_back_instead_of_churning_event_taps() {
        let started = std::time::Instant::now();
        let mut guard = HandyKeysRestartGuard::default();

        assert!(guard.reserve_restart(started));
        assert!(!guard.reserve_restart(
            started + HANDY_KEYS_RESTART_COOLDOWN - std::time::Duration::from_millis(1)
        ));
        assert!(guard.reserve_restart(started + HANDY_KEYS_RESTART_COOLDOWN));

        guard.reset();
        assert!(guard.reserve_restart(started));
    }

    #[test]
    fn disabled_post_process_binding_is_persisted_without_runtime_registration() {
        let mut app_settings = settings::get_default_settings();
        app_settings.post_process_enabled = false;

        assert!(binding_is_runtime_active(&app_settings, "transcribe"));
        assert!(!binding_is_runtime_active(
            &app_settings,
            "transcribe_with_post_process"
        ));
        assert!(!binding_is_runtime_active(&app_settings, "cancel"));

        app_settings.post_process_enabled = true;
        assert!(binding_is_runtime_active(
            &app_settings,
            "transcribe_with_post_process"
        ));
    }

    #[test]
    fn shortcut_changes_are_persistence_only_while_capture_is_suspended() {
        let mut app_settings = settings::get_default_settings();

        assert!(binding_change_updates_runtime(
            &app_settings,
            "transcribe",
            false
        ));
        assert!(!binding_change_updates_runtime(
            &app_settings,
            "transcribe",
            true
        ));

        app_settings.post_process_enabled = false;
        assert!(!binding_change_updates_runtime(
            &app_settings,
            "transcribe_with_post_process",
            false
        ));
    }

    #[test]
    fn failed_capture_resume_runs_deterministic_recovery() {
        let recovery_called = std::cell::Cell::new(false);

        let result = recover_failed_shortcut_resume(Err("registration failed".to_string()), || {
            recovery_called.set(true);
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert!(recovery_called.get());
    }

    #[test]
    fn failed_capture_resume_reports_recovery_failure_without_hiding_cause() {
        let result = recover_failed_shortcut_resume(Err("registration failed".to_string()), || {
            Err("fallback failed".to_string())
        });

        assert_eq!(
            result,
            Err(
                "Could not restore suspended shortcuts (registration failed); permission-free recovery failed (fallback failed)"
                    .to_string()
            )
        );
    }

    #[test]
    fn shortcut_capture_suspend_restores_already_removed_bindings_on_failure() {
        let bindings = vec![("first", 1), ("second", 2), ("third", 3)];
        let suspended = std::cell::RefCell::new(Vec::new());
        let restored = std::cell::RefCell::new(Vec::new());

        let result = suspend_bindings_transactionally(
            bindings,
            |binding| {
                if *binding == 2 {
                    Err("native unregister failed".to_string())
                } else {
                    suspended.borrow_mut().push(*binding);
                    Ok(())
                }
            },
            |binding| {
                restored.borrow_mut().push(*binding);
                Ok(())
            },
        );

        assert!(result
            .unwrap_err()
            .contains("Could not suspend shortcut 'second'"));
        assert_eq!(*suspended.borrow(), vec![1]);
        assert_eq!(*restored.borrow(), vec![1]);
    }

    #[test]
    fn shortcut_capture_resume_rolls_back_partial_success_for_retry() {
        let bindings = vec![("first", 1), ("second", 2), ("third", 3)];
        let resumed = std::cell::RefCell::new(Vec::new());
        let rolled_back = std::cell::RefCell::new(Vec::new());

        let result = resume_bindings_transactionally(
            bindings,
            |binding| {
                if *binding == 2 {
                    Err("native register failed".to_string())
                } else {
                    resumed.borrow_mut().push(*binding);
                    Ok(())
                }
            },
            |binding| {
                rolled_back.borrow_mut().push(*binding);
                Ok(())
            },
        );

        assert!(result
            .unwrap_err()
            .contains("Could not restore shortcut 'second'"));
        assert_eq!(*resumed.borrow(), vec![1]);
        assert_eq!(*rolled_back.borrow(), vec![1]);
    }

    #[test]
    fn delayed_cancel_registration_reloads_the_profile_after_fallback() {
        let mut restricted = settings::get_default_settings();
        restricted.keyboard_implementation = KeyboardImplementation::Tauri;
        restricted.paste_method = PasteMethod::None;
        restricted
            .bindings
            .get_mut("cancel")
            .unwrap()
            .current_binding = "MediaPlayPause".into();

        let profile = std::cell::RefCell::new(restricted);
        let fallback_count = std::cell::Cell::new(0);
        let registered = std::cell::RefCell::new(None);

        register_cancel_shortcut_with(
            || profile.borrow().clone(),
            || false,
            || {
                fallback_count.set(fallback_count.get() + 1);
                profile.borrow_mut().use_minimum_permissions_profile();
                Ok(())
            },
            |implementation, binding| {
                *registered.borrow_mut() = Some((implementation, binding.current_binding));
                Ok(())
            },
        )
        .expect("permission-free cancel shortcut should register");

        assert_eq!(fallback_count.get(), 1);
        assert_eq!(
            registered.into_inner(),
            Some((
                KeyboardImplementation::Tauri,
                settings::get_default_settings().bindings["cancel"]
                    .current_binding
                    .clone()
            ))
        );
    }

    #[test]
    fn ordinary_cancel_registration_does_not_need_accessibility() {
        let mut permission_free = settings::get_default_settings();
        permission_free.use_minimum_permissions_profile();
        let fallback_called = std::cell::Cell::new(false);

        register_cancel_shortcut_with(
            || permission_free.clone(),
            || false,
            || {
                fallback_called.set(true);
                Ok(())
            },
            |implementation, binding| {
                assert_eq!(implementation, KeyboardImplementation::Tauri);
                assert!(settings::tauri_shortcut_is_permission_free_on_macos(
                    &binding.current_binding
                ));
                Ok(())
            },
        )
        .expect("ordinary Tauri cancel shortcut should remain available");

        assert!(!fallback_called.get());
    }

    #[test]
    fn newest_cancel_request_wins_when_tasks_execute_out_of_order() {
        let requests = CancelShortcutRequestTracker::new();
        let start = requests.next(true);
        let stop = requests.next(false);

        assert!(!requests.is_current(start));
        assert!(requests.is_current(stop));
        assert!(start.desired);
        assert!(!stop.desired);
    }

    #[test]
    fn accessibility_gate_rejects_restricted_settings_without_permission() {
        let mut app_settings = settings::get_default_settings();
        app_settings.keyboard_implementation = KeyboardImplementation::HandyKeys;
        app_settings.paste_method = PasteMethod::None;
        app_settings.auto_submit = false;

        let error = validate_macos_accessibility_configuration(&app_settings, false)
            .expect_err("HandyKeys must be rejected without Accessibility");

        assert!(error.contains("Accessibility"));
    }

    #[test]
    fn accessibility_gate_accepts_manual_paste_and_ordinary_shortcuts() {
        let mut app_settings = settings::get_default_settings();
        app_settings.use_minimum_permissions_profile();

        assert!(validate_macos_accessibility_configuration(&app_settings, false).is_ok());
    }

    #[test]
    fn accessibility_gate_accepts_restricted_settings_after_permission_grant() {
        let app_settings = settings::get_default_settings();

        assert!(app_settings.requires_macos_accessibility());
        assert!(validate_macos_accessibility_configuration(&app_settings, true).is_ok());
    }

    #[test]
    fn shortcut_gate_rejects_handy_keys_and_tauri_media_keys_without_permission() {
        let ordinary = ShortcutBinding {
            id: "transcribe".into(),
            name: "Transcribe".into(),
            description: "Start transcription".into(),
            default_binding: "option+space".into(),
            current_binding: "option+space".into(),
        };
        let media = ShortcutBinding {
            current_binding: "MediaPlayPause".into(),
            ..ordinary.clone()
        };

        assert!(
            validate_shortcut_accessibility(KeyboardImplementation::Tauri, &ordinary, false)
                .is_ok()
        );
        assert!(
            validate_shortcut_accessibility(KeyboardImplementation::Tauri, &media, false).is_err()
        );
        assert!(validate_shortcut_accessibility(
            KeyboardImplementation::HandyKeys,
            &ordinary,
            false
        )
        .is_err());
    }

    #[test]
    fn switching_from_handy_keys_after_revocation_normalizes_the_full_profile() {
        let mut app_settings = settings::get_default_settings();
        app_settings.keyboard_implementation = KeyboardImplementation::HandyKeys;
        app_settings.paste_method = PasteMethod::CtrlV;
        app_settings.auto_submit = true;
        app_settings
            .bindings
            .get_mut("transcribe")
            .unwrap()
            .current_binding = "MediaPlayPause".into();

        let reset = prepare_keyboard_implementation_change(
            &mut app_settings,
            KeyboardImplementation::Tauri,
            false,
        )
        .expect("permission-free fallback should always be available");

        assert_eq!(
            app_settings.keyboard_implementation,
            KeyboardImplementation::Tauri
        );
        assert_eq!(app_settings.paste_method, PasteMethod::None);
        assert_eq!(
            app_settings.clipboard_handling,
            ClipboardHandling::CopyToClipboard
        );
        assert!(!app_settings.auto_submit);
        assert_eq!(reset, vec!["transcribe"]);
        assert!(!app_settings.requires_macos_accessibility());
    }

    #[test]
    fn switching_to_handy_keys_is_rejected_before_settings_change_without_permission() {
        let mut app_settings = settings::get_default_settings();
        app_settings.use_minimum_permissions_profile();

        let result = prepare_keyboard_implementation_change(
            &mut app_settings,
            KeyboardImplementation::HandyKeys,
            false,
        );

        assert!(result.is_err());
        // The helper mutates only a candidate settings value; callers never
        // persist it after this error.
        assert_eq!(
            app_settings.keyboard_implementation,
            KeyboardImplementation::HandyKeys
        );
    }

    #[test]
    fn synthetic_input_gate_allows_manual_paste_without_permission() {
        assert!(validate_synthetic_input_accessibility(PasteMethod::None, false, false).is_ok());
        assert!(validate_synthetic_input_accessibility(PasteMethod::None, true, false).is_ok());
        assert!(
            validate_synthetic_input_accessibility(PasteMethod::ExternalScript, false, false)
                .is_ok()
        );
    }

    #[test]
    fn synthetic_input_gate_rejects_automatic_paste_without_permission() {
        for method in [
            PasteMethod::CtrlV,
            PasteMethod::Direct,
            PasteMethod::ShiftInsert,
            PasteMethod::CtrlShiftV,
        ] {
            assert!(
                validate_synthetic_input_accessibility(method, false, false).is_err(),
                "{method:?} should require Accessibility"
            );
        }

        assert!(
            validate_synthetic_input_accessibility(PasteMethod::ExternalScript, true, false)
                .is_err()
        );
    }

    #[test]
    fn minimum_permissions_profile_resets_custom_shortcuts_to_safe_defaults() {
        let mut app_settings = settings::get_default_settings();
        app_settings
            .bindings
            .get_mut("transcribe")
            .unwrap()
            .current_binding = "MediaPlayPause".into();
        app_settings
            .bindings
            .get_mut("transcribe_with_post_process")
            .unwrap()
            .current_binding = "command+shift+keyq".into();
        app_settings
            .bindings
            .get_mut("cancel")
            .unwrap()
            .current_binding = "option".into();

        let reset = prepare_minimum_permissions_profile(&mut app_settings);

        assert_eq!(reset, vec!["cancel", "transcribe"]);
        assert_eq!(
            app_settings.bindings["transcribe"].current_binding,
            settings::get_default_settings().bindings["transcribe"].current_binding
        );
        assert_eq!(
            app_settings.bindings["transcribe_with_post_process"].current_binding,
            "command+shift+keyq"
        );
        assert_eq!(
            app_settings.bindings["cancel"].current_binding,
            settings::get_default_settings().bindings["cancel"].current_binding
        );
        assert!(!app_settings.requires_macos_accessibility());
    }

    #[test]
    fn minimum_permissions_profile_restores_missing_shortcuts() {
        let mut app_settings = settings::get_default_settings();
        app_settings.bindings.remove("cancel");

        let reset = prepare_minimum_permissions_profile(&mut app_settings);

        assert_eq!(reset, vec!["cancel"]);
        assert_eq!(
            app_settings.bindings["cancel"].current_binding,
            settings::get_default_settings().bindings["cancel"].current_binding
        );
    }

    #[test]
    fn failed_backend_recovery_resets_custom_shortcuts_to_defaults() {
        let mut app_settings = settings::get_default_settings();
        app_settings
            .bindings
            .get_mut("transcribe")
            .unwrap()
            .current_binding = "command+shift+keyq".into();

        let reset = prepare_default_minimum_permissions_profile(&mut app_settings);

        assert_eq!(reset, vec!["transcribe"]);
        assert_eq!(
            app_settings.bindings["transcribe"].current_binding,
            settings::get_default_settings().bindings["transcribe"].current_binding
        );
        assert!(!app_settings.requires_macos_accessibility());
    }

    #[test]
    fn bulk_shortcut_operations_include_only_active_static_bindings() {
        let mut app_settings = settings::get_default_settings();

        let disabled_ids: Vec<_> = active_static_bindings(&app_settings)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(disabled_ids, vec!["transcribe"]);

        app_settings.post_process_enabled = true;
        let enabled_ids: Vec<_> = active_static_bindings(&app_settings)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            enabled_ids,
            vec!["transcribe", "transcribe_with_post_process"]
        );
        assert!(!enabled_ids.contains(&"cancel"));
    }
}
