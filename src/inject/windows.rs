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

/// Nothing to compare on Windows; kept so the daemon's guard compiles unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus;

pub fn focus() -> Option<Focus> {
    None
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
pub fn paste(method: PasteMethod, _focus: Option<&Focus>) -> Result<PasteMethod> {
    if method == PasteMethod::None {
        return Ok(PasteMethod::None);
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

/// No toast backend yet; the transcript is on the clipboard either way.
pub fn notify(summary: &str, body: &str) {
    crate::log::write(&format!("{summary}: {body}"));
}
