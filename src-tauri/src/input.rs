use enigo::{Direction, Enigo, Key, Keyboard, Mouse, Settings};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

/// Wrapper for Enigo to store in Tauri's managed state.
/// Enigo is wrapped in a Mutex since it requires mutable access.
pub struct EnigoState(pub Mutex<Enigo>);

impl EnigoState {
    pub fn new() -> Result<Self, String> {
        // Permission requests are an explicit UI action. Enigo's default is to
        // trigger the macOS prompt during construction, which made an optional
        // feature look like an application-wide requirement.
        let settings = Settings {
            open_prompt_to_get_permissions: false,
            ..Settings::default()
        };
        let enigo =
            Enigo::new(&settings).map_err(|e| format!("Failed to initialize Enigo: {}", e))?;
        Ok(Self(Mutex::new(enigo)))
    }
}

/// Get the current mouse cursor position using the managed Enigo instance.
/// Returns None if the state is not available or if getting the location fails.
pub fn get_cursor_position(app_handle: &AppHandle) -> Option<(i32, i32)> {
    let enigo_state = app_handle.try_state::<EnigoState>()?;
    let enigo = enigo_state.0.lock().ok()?;
    enigo.location().ok()
}

/// Send a key chord while guaranteeing best-effort release of every modifier
/// that was successfully pressed. Input permission can disappear between two
/// native calls, so early-returning on the trigger error can otherwise leave a
/// system modifier logically held.
pub(crate) fn send_key_chord(
    modifiers: &[Key],
    trigger: Key,
    hold_ms: u64,
    mut send: impl FnMut(Key, Direction) -> Result<(), String>,
) -> Result<(), String> {
    let mut held_modifiers = Vec::with_capacity(modifiers.len());
    let mut first_error = None;

    for &modifier in modifiers {
        match send(modifier, Direction::Press) {
            Ok(()) => held_modifiers.push(modifier),
            Err(error) => {
                first_error = Some(error);
                break;
            }
        }
    }

    if first_error.is_none() {
        if let Err(error) = send(trigger, Direction::Click) {
            first_error = Some(error);
        } else {
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        }
    }

    for modifier in held_modifiers.into_iter().rev() {
        if let Err(error) = send(modifier, Direction::Release) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }

    first_error.map_or(Ok(()), Err)
}

fn send_enigo_chord(
    enigo: &mut Enigo,
    modifiers: &[Key],
    trigger: Key,
    hold_ms: u64,
) -> Result<(), String> {
    send_key_chord(modifiers, trigger, hold_ms, |key, direction| {
        enigo
            .key(key, direction)
            .map_err(|error| format!("Failed to send {direction:?} for {key:?}: {error}"))
    })
}

/// Sends a Ctrl+V or Cmd+V paste command using platform-specific virtual key codes.
/// This ensures the paste works regardless of keyboard layout (e.g., Russian, AZERTY, DVORAK).
/// Note: On Wayland, this may not work - callers should check for Wayland and use alternative methods.
///
/// `hold_ms` is how long the modifier stays held after the V click before being
/// released. Most applications read the modifier from the V event's flags and
/// need no hold at all, but applications that poll global keyboard state when
/// handling the key need the modifier to still be down — the hold insures
/// against those. Callers that can detect a failed chord (e.g. the
/// receipt-sequenced paste path) may use a much shorter hold.
pub fn send_paste_ctrl_v(enigo: &mut Enigo, hold_ms: u64) -> Result<(), String> {
    // Platform-specific key definitions
    #[cfg(target_os = "macos")]
    let (modifier_key, v_key_code) = (Key::Meta, Key::Other(9));
    #[cfg(target_os = "windows")]
    let (modifier_key, v_key_code) = (Key::Control, Key::Other(0x56)); // VK_V
    #[cfg(target_os = "linux")]
    let (modifier_key, v_key_code) = (Key::Control, Key::Unicode('v'));

    send_enigo_chord(enigo, &[modifier_key], v_key_code, hold_ms)
}

/// Sends a Ctrl+Shift+V paste command.
/// This is commonly used in terminal applications on Linux to paste without formatting.
/// Note: On Wayland, this may not work - callers should check for Wayland and use alternative methods.
pub fn send_paste_ctrl_shift_v(enigo: &mut Enigo, hold_ms: u64) -> Result<(), String> {
    // Platform-specific key definitions
    #[cfg(target_os = "macos")]
    let (modifier_key, v_key_code) = (Key::Meta, Key::Other(9)); // Cmd+Shift+V on macOS
    #[cfg(target_os = "windows")]
    let (modifier_key, v_key_code) = (Key::Control, Key::Other(0x56)); // VK_V
    #[cfg(target_os = "linux")]
    let (modifier_key, v_key_code) = (Key::Control, Key::Unicode('v'));

    send_enigo_chord(enigo, &[modifier_key, Key::Shift], v_key_code, hold_ms)
}

/// Sends a Shift+Insert paste command (Windows and Linux only).
/// This is more universal for terminal applications and legacy software.
/// Note: On Wayland, this may not work - callers should check for Wayland and use alternative methods.
pub fn send_paste_shift_insert(enigo: &mut Enigo, hold_ms: u64) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let insert_key_code = Key::Other(0x2D); // VK_INSERT
    #[cfg(not(target_os = "windows"))]
    let insert_key_code = Key::Other(0x76); // XK_Insert (keycode 118 / 0x76, also used as fallback)

    send_enigo_chord(enigo, &[Key::Shift], insert_key_code, hold_ms)
}

/// Pastes text directly using the enigo text method.
/// This tries to use system input methods if possible, otherwise simulates keystrokes one by one.
pub fn paste_text_direct(enigo: &mut Enigo, text: &str) -> Result<(), String> {
    enigo
        .text(text)
        .map_err(|e| format!("Failed to send text directly: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_releases_held_modifier_when_key_click_fails() {
        let mut actions = Vec::new();

        let result = send_key_chord(&[Key::Control], Key::Unicode('v'), 0, |key, direction| {
            actions.push((key, direction));
            if direction == Direction::Click {
                Err("click failed".to_string())
            } else {
                Ok(())
            }
        });

        assert_eq!(result, Err("click failed".to_string()));
        assert_eq!(
            actions,
            vec![
                (Key::Control, Direction::Press),
                (Key::Unicode('v'), Direction::Click),
                (Key::Control, Direction::Release),
            ]
        );
    }

    #[test]
    fn chord_attempts_every_release_after_a_release_failure() {
        let mut actions = Vec::new();

        let result = send_key_chord(
            &[Key::Control, Key::Shift],
            Key::Unicode('v'),
            0,
            |key, direction| {
                actions.push((key, direction));
                if key == Key::Shift && direction == Direction::Release {
                    Err("shift release failed".to_string())
                } else {
                    Ok(())
                }
            },
        );

        assert_eq!(result, Err("shift release failed".to_string()));
        assert_eq!(
            &actions[actions.len() - 2..],
            &[
                (Key::Shift, Direction::Release),
                (Key::Control, Direction::Release),
            ]
        );
    }
}
