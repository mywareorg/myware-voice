//! Push-to-talk dictation.
//!
//! The daemon owns the microphone and a warm recognizer; `hippo-voice toggle`
//! is what the compositor keybind runs. Nothing here opens a window.

pub mod audio;
pub mod capture;
pub mod config;
pub mod daemon;
pub mod dirs;
#[cfg(windows)]
pub mod hotkey;
pub mod inject;
pub mod log;
pub mod model;
pub mod sherpa;
pub mod tone;
pub mod transport;
