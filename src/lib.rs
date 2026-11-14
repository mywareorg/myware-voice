//! Push-to-talk dictation.
//!
//! The daemon owns the microphone and a warm recognizer; `hippo-dictate toggle`
//! is what the compositor keybind runs. Nothing here opens a window.

pub mod audio;
pub mod capture;
pub mod chord;
pub mod config;
pub mod daemon;
pub mod dirs;
pub mod history;
#[cfg(windows)]
pub mod hotkey;
pub mod inject;
pub mod licence;
pub mod log;
pub mod model;
#[cfg(windows)]
pub mod overlay;
pub mod sherpa;
pub mod text;
pub mod tone;
pub mod transport;
#[cfg(windows)]
pub mod tray;
