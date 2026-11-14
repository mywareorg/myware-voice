//! Push-to-talk dictation.
//!
//! The daemon owns the microphone and a warm recognizer; `hippo-voice toggle`
//! is what the compositor keybind runs. Nothing here opens a window.

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
/// The audio preparation the models expect, from the shared integration.
pub use hippo_sherpa::audio;

pub mod model;
#[cfg(windows)]
pub mod overlay;
pub mod sherpa;
pub mod text;
pub mod tone;
pub mod transport;
#[cfg(windows)]
pub mod tray;
