//! Reads the text the user has selected in whatever app is in front.
//!
//! - **Windows:** UI Automation asks the focused control for its selection
//!   directly. No permission prompt and no clipboard involvement. Apps that
//!   don't implement UIA's TextPattern fall back to copying.
//! - **Linux:** the PRIMARY selection, which X11 and most Wayland compositors
//!   fill whenever text is highlighted. (GNOME's Wayland session doesn't expose
//!   it to other apps.)
//! - **macOS:** copies the selection (Cmd+C) and restores the clipboard.
//!   Reading `AXSelectedText` would avoid the clipboard but needs the
//!   Accessibility permission; not done yet.

#[cfg(target_os = "linux")]
use log::debug;
#[cfg(target_os = "windows")]
use log::info;
use tauri::AppHandle;

/// The selected text, cleaned of surrounding whitespace, or `None` when
/// nothing is selected.
pub fn selected_text(app: &AppHandle) -> Option<String> {
    let text = platform_selected_text(app)?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(target_os = "linux")]
fn platform_selected_text(_app: &AppHandle) -> Option<String> {
    use arboard::{Clipboard, GetExtLinux, LinuxClipboardKind};
    match Clipboard::new().and_then(|mut c| c.get().clipboard(LinuxClipboardKind::Primary).text()) {
        Ok(text) => Some(text),
        Err(e) => {
            debug!("PRIMARY selection unavailable via arboard ({e}); trying wl-paste");
            let out = std::process::Command::new("wl-paste")
                .args(["--primary", "--no-newline"])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

#[cfg(target_os = "windows")]
fn platform_selected_text(app: &AppHandle) -> Option<String> {
    match uia::selected_text_with_timeout(std::time::Duration::from_millis(1500)) {
        uia::Outcome::Text(text) => {
            info!("Read selection via UI Automation ({} chars)", text.len());
            Some(text)
        }
        // The control supports text selection and reports none: copying would
        // only grab something unintended (VS Code copies the whole line).
        uia::Outcome::NothingSelected => None,
        uia::Outcome::Unsupported(reason) => {
            info!("UI Automation can't read this app's selection ({reason}); copying instead");
            copy::copy_selection(app)
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_selected_text(app: &AppHandle) -> Option<String> {
    if !crate::accessibility::is_granted() {
        return None;
    }
    copy::copy_selection(app)
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn platform_selected_text(_app: &AppHandle) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
mod uia {
    use std::sync::mpsc;
    use std::time::Duration;
    use windows::core::Interface;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId,
    };

    pub enum Outcome {
        Text(String),
        NothingSelected,
        Unsupported(String),
    }

    /// How far up from the focused element to look for a TextPattern.
    /// Browsers often focus a child of the document that exposes the text.
    const MAX_ANCESTORS: usize = 6;

    /// UIA calls go to the target app's process and can hang if it's busy,
    /// so they run on their own thread with a deadline.
    pub fn selected_text_with_timeout(timeout: Duration) -> Outcome {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read_selection());
        });
        rx.recv_timeout(timeout)
            .unwrap_or_else(|_| Outcome::Unsupported("timed out".into()))
    }

    fn read_selection() -> Outcome {
        unsafe {
            let com = CoInitializeEx(None, COINIT_MULTITHREADED);
            let result = read_selection_inner();
            if com.is_ok() {
                CoUninitialize();
            }
            result.unwrap_or_else(|e| Outcome::Unsupported(e.to_string()))
        }
    }

    unsafe fn read_selection_inner() -> windows::core::Result<Outcome> {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let walker = automation.ControlViewWalker()?;
        let mut element = automation.GetFocusedElement()?;

        for _ in 0..MAX_ANCESTORS {
            if let Ok(pattern) =
                element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            {
                if pattern.as_raw().is_null() {
                    // Treat a null pattern the same as a missing one.
                } else {
                    let ranges = pattern.GetSelection()?;
                    let mut text = String::new();
                    for i in 0..ranges.Length()? {
                        let range = ranges.GetElement(i)?;
                        text.push_str(&range.GetText(-1)?.to_string());
                    }
                    return Ok(if text.trim().is_empty() {
                        Outcome::NothingSelected
                    } else {
                        Outcome::Text(text)
                    });
                }
            }
            match walker.GetParentElement(&element) {
                Ok(parent) => element = parent,
                Err(_) => break,
            }
        }
        Ok(Outcome::Unsupported(
            "no TextPattern on the focused control".into(),
        ))
    }
}

/// Fallback: copy the selection with the platform shortcut, read it, and put
/// the user's clipboard back.
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod copy {
    use log::warn;
    use std::time::{Duration, Instant};
    use tauri::AppHandle;
    use tauri_plugin_clipboard_manager::ClipboardExt;

    pub fn copy_selection(app: &AppHandle) -> Option<String> {
        let clipboard = app.clipboard();
        let saved = clipboard.read_text().ok();
        // Clear first so a stale clipboard isn't mistaken for the selection.
        if let Err(e) = clipboard.write_text("") {
            warn!("Couldn't clear the clipboard before copying: {e}");
            return None;
        }

        wait_for_modifiers_released(Duration::from_millis(1000));
        if let Err(e) = crate::clipboard::with_enigo(app, |enigo| send_copy(enigo)) {
            warn!("Couldn't send the copy shortcut: {e}");
        }

        let deadline = Instant::now() + Duration::from_millis(500);
        let mut copied = None;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            if let Ok(text) = clipboard.read_text() {
                if !text.is_empty() {
                    copied = Some(text);
                    break;
                }
            }
        }

        // Only text is restored; an image on the clipboard is lost.
        let _ = clipboard.write_text(saved.unwrap_or_default());
        copied
    }

    fn send_copy(enigo: &mut enigo::Enigo) -> Result<(), String> {
        use enigo::{Direction, Key, Keyboard};
        #[cfg(target_os = "macos")]
        let (modifier, c_key) = (Key::Meta, Key::Other(8)); // kVK_ANSI_C
        #[cfg(target_os = "windows")]
        let (modifier, c_key) = (Key::Control, Key::Other(0x43)); // VK_C
        enigo
            .key(modifier, Direction::Press)
            .map_err(|e| e.to_string())?;
        let click = enigo
            .key(c_key, Direction::Click)
            .map_err(|e| e.to_string());
        std::thread::sleep(Duration::from_millis(30));
        enigo
            .key(modifier, Direction::Release)
            .map_err(|e| e.to_string())?;
        click
    }

    /// The hotkey's own modifiers would otherwise turn Ctrl+C into something
    /// like Ctrl+Alt+C.
    #[cfg(target_os = "windows")]
    fn wait_for_modifiers_released(max: Duration) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
        };
        let deadline = Instant::now() + max;
        while Instant::now() < deadline {
            let held = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
                .iter()
                .any(|vk| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0);
            if !held {
                return;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    #[cfg(target_os = "macos")]
    fn wait_for_modifiers_released(_max: Duration) {
        std::thread::sleep(Duration::from_millis(200));
    }
}
