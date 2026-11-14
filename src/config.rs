//! User configuration, read from one TOML file.
//!
//! Everything the daemon needs to be tunable without a rebuild lives here. The
//! file is optional: a missing file means the defaults.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How the transcript reaches the focused window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasteMethod {
    /// Try `wtype`, then `ydotool`, then fall back to the clipboard.
    #[default]
    Auto,
    /// `wtype -M ctrl v -m ctrl` — needs the virtual-keyboard protocol.
    Wtype,
    /// `ydotool key 29:1 47:1 47:0 29:0` — needs `ydotoold` and uinput access.
    Ydotool,
    /// Only copy; leave the paste to the user.
    None,
}

impl core::fmt::Display for PasteMethod {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.arg())
    }
}

impl PasteMethod {
    pub const ALL: [PasteMethod; 4] = [
        PasteMethod::Auto,
        PasteMethod::Wtype,
        PasteMethod::Ydotool,
        PasteMethod::None,
    ];

    pub fn arg(self) -> &'static str {
        match self {
            PasteMethod::Auto => "auto",
            PasteMethod::Wtype => "wtype",
            PasteMethod::Ydotool => "ydotool",
            PasteMethod::None => "none",
        }
    }

    pub fn from_arg(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(PasteMethod::Auto),
            "wtype" => Some(PasteMethod::Wtype),
            "ydotool" => Some(PasteMethod::Ydotool),
            "none" => Some(PasteMethod::None),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Input device name, or `None` for the host default.
    pub device: Option<String>,
    /// How the transcript is delivered.
    pub paste: PasteMethod,
    /// Split a recording at silences into chunks this long. Parakeet degrades on
    /// long single clips and grows memory, so this is a correctness limit.
    pub chunk_seconds: f32,
    /// Drop the warm recognizer after this many minutes idle. Zero keeps it.
    pub idle_unload_minutes: u32,
    /// Append a space to each transcript so consecutive dictations concatenate.
    pub trailing_space: bool,
    /// Hard stop for one capture, in seconds.
    pub max_seconds: f32,
    /// Play a tone when recording starts and when a transcript is delivered.
    pub tones: bool,
    /// The chord the daemon registers on Windows. Linux uses the compositor's
    /// binding and ignores this.
    ///
    /// `ctrl+space` is the product default. While the daemon runs that chord is
    /// owned system-wide, so the input-method switch on some layouts and the
    /// autocomplete key in most editors stop reaching their applications. Change
    /// it here if that matters more than the shorter chord.
    pub hotkey: String,
    /// The chord that starts and stops streaming dictation on Windows.
    pub stream_hotkey: String,
    /// Target seconds per fallback phrase when the online streaming model is
    /// not installed. Streaming waits for a pause near this length before
    /// committing, so a word is not cut in two. The word-by-word model ignores
    /// it.
    pub stream_seconds: f32,
    /// Force a streaming phrase handoff after this many seconds even when the
    /// speaker never pauses. The online encoder's context is unbounded on paper
    /// but grows without limit in practice, and a monologue with no endpoint
    /// would otherwise keep one head alive for the whole session.
    pub stream_segment_seconds: f32,
    /// Keep delivered transcripts at rest. Off by default: the app's premise is
    /// that transcripts stay where the user puts them.
    pub history: bool,
    /// Delete history entries older than this many days.
    pub history_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: None,
            paste: PasteMethod::Auto,
            chunk_seconds: 20.0,
            idle_unload_minutes: 30,
            trailing_space: true,
            max_seconds: 300.0,
            tones: true,
            hotkey: "ctrl+space".to_string(),
            stream_hotkey: "ctrl+shift+space".to_string(),
            stream_seconds: 3.0,
            stream_segment_seconds: 8.0,
            history: false,
            history_days: 30,
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        crate::dirs::config_file()
    }

    /// Load, or return the defaults when there is no file.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_usable_and_round_trip_through_toml() {
        let config = Config::default();
        let text = toml::to_string(&config).expect("serialize");
        let back: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(back.chunk_seconds, config.chunk_seconds);
        assert_eq!(back.paste, config.paste);
        assert_eq!(PasteMethod::from_arg("ydotool"), Some(PasteMethod::Ydotool));
    }

    #[test]
    fn a_partial_file_keeps_the_other_defaults() {
        let parsed: Config = toml::from_str("paste = \"none\"\n").expect("parse");
        assert_eq!(parsed.paste, PasteMethod::None);
        assert_eq!(parsed.chunk_seconds, Config::default().chunk_seconds);
    }
}
