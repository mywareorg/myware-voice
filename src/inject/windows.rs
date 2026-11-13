//! Windows delivery: the clipboard through `arboard`, the paste chord through
//! `SendInput` (via `enigo`).
//!
//! There is no per-window focus identity to compare the way sway exposes one.
//! `SendInput` goes to whatever window is foreground at the moment it runs, which
//! is the same window the user was typing in because the daemon never takes
//! focus.

use anyhow::{Context, Result};
use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use crate::config::PasteMethod;

/// The foreground window at the moment the recording stopped.
///
/// Without this the guard compares `None` with `None` and never fires, so a
/// decode that takes a second could paste into whatever window the user switched
/// to in the meantime - the one failure a dictation tool must not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Focus(usize);

pub fn focus() -> Option<Focus> {
    let handle = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    if handle.is_null() {
        None
    } else {
        Some(Focus(handle as usize))
    }
}

impl Focus {
    /// Windows terminals paste with a plain `ctrl+v`, so there is no shifted
    /// chord to choose. Kept for symmetry with the Unix backend.
    pub fn is_terminal(&self) -> bool {
        false
    }
}

/// Every paste method means the same thing here.
pub fn resolve(method: PasteMethod) -> PasteMethod {
    method
}

/// Put `text` on the clipboard, without a trailing newline.
pub fn copy(text: &str) -> Result<()> {
    let text = text.trim_end_matches(['\n', '\r']);
    let mut clipboard = Clipboard::new().context("open the clipboard")?;
    clipboard
        .set_text(text.to_string())
        .context("set clipboard text")?;
    Ok(())
}

/// Send `ctrl+v` to the foreground window.
///
/// An elevated foreground window will not accept injected input (UIPI). Nothing
/// can be done about that from a normal-privilege process: the transcript stays on
/// the clipboard and the caller's fallback tells the user to paste it.
pub fn paste(method: PasteMethod, _focus: Option<&Focus>) -> Result<PasteMethod> {
    if method == PasteMethod::None {
        return Ok(PasteMethod::None);
    }

    // UIPI silently discards injected input aimed at a higher-integrity window,
    // so an elevated terminal would look like a dead keybind. Detect it first and
    // leave the transcript on the clipboard with a message instead.
    if foreground_is_elevated(_focus) {
        anyhow::bail!("the focused window is running elevated; Windows blocks injected input");
    }

    let mut enigo = Enigo::new(&Settings::default()).context("start input injection")?;
    enigo
        .key(Key::Control, Direction::Press)
        .context("press ctrl")?;
    enigo.key(Key::Unicode('v'), Direction::Click)?;
    enigo
        .key(Key::Control, Direction::Release)
        .context("release ctrl")?;
    Ok(PasteMethod::Auto)
}

/// Whether the focused window belongs to an elevated process.
///
/// Returns false whenever the query is not conclusive: a failed check must not
/// stop an ordinary paste.
fn foreground_is_elevated(focus: Option<&Focus>) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HWND};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    let Some(Focus(handle)) = focus else {
        return false;
    };

    unsafe {
        let window = *handle as HWND;
        let mut process_id = 0u32;
        GetWindowThreadProcessId(window, &mut process_id);
        if process_id == 0 {
            return false;
        }

        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return false;
        }

        let mut token = std::ptr::null_mut();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        CloseHandle(process);
        if opened == 0 {
            return false;
        }

        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut returned = 0u32;
        let asked = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);

        asked != 0 && elevation.TokenIsElevated != 0
    }
}

/// A balloon from the tray icon, so the fallback message is visible.
pub fn notify(summary: &str, body: &str) {
    crate::log::write(&format!("{summary}: {body}"));
    // A balloon from the tray icon: without it the fallback message would only
    // exist in a log file nobody opens.
    crate::tray::notify(summary, body);
}
