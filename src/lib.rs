//! Push-to-talk dictation.
//!
//! The daemon owns the microphone and a warm recognizer; `hippo-voice toggle`
//! is what the compositor keybind runs. Nothing here opens a window.

pub mod audio;
pub mod capture;
pub mod config;
pub mod daemon;
pub mod inject;
pub mod sherpa;
pub mod tone;
