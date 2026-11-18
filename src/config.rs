//! User configuration, read from one TOML file.
//!
//! Everything the daemon needs to be tunable without a rebuild lives here. The
//! file is optional: a missing file means the defaults.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

/// A phrase that expands to fixed text when it is the whole utterance.
///
/// `trigger` may hold several phrases separated by commas; any of them expands
/// to `text`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snippet {
    pub trigger: String,
    pub text: String,
}

/// A whole-word substitution: a name, an acronym, a correction.
///
/// `from` may hold several phrases separated by commas; each is replaced by `to`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replacement {
    pub from: String,
    pub to: String,
}

/// A spoken form and what it becomes.
///
/// `say` is matched whole-word and case-insensitively; `becomes` is inserted
/// verbatim, so a rule that wants a following space carries it. A comma in `say`
/// separates alternatives, all of which produce the same `becomes`, which is why
/// the defaults read "exclamation mark, exclamation point". `becomes` is never
/// split. These are the tables the text pass uses, and they are in the config
/// file so a user can see every rule and change any of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpokenRule {
    pub say: String,
    pub becomes: String,
}

/// A whole-utterance instruction: what to say, and what to do about it.
///
/// `say` may hold several phrases separated by commas, any of which fires the
/// action. `action` is one of `discard`, `quit`, `bullets`, `one line`. An action
/// this build does not know is ignored rather than treated as text, which is how
/// a config from a newer version degrades.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRule {
    pub say: String,
    pub action: String,
}

fn default_fillers() -> Vec<String> {
    ["um", "uh", "uhm", "erm", "hmm", "mhm"]
        .iter()
        .map(|word| word.to_string())
        .collect()
}

fn default_punctuation() -> Vec<SpokenRule> {
    // One row per mark; the comma-separated spellings are alternatives for the
    // same `becomes`, which is why "exclamation point" is not a second rule.
    const TABLE: [(&str, &str); 13] = [
        ("open parenthesis", "("),
        ("close parenthesis", ")"),
        ("exclamation mark, exclamation point", "! "),
        ("question mark", "? "),
        ("new paragraph", "\n\n"),
        ("new line", "\n"),
        ("full stop, period", ". "),
        ("open quote, close quote", "\""),
        ("semicolon", "; "),
        ("hyphen", "-"),
        ("comma", ", "),
        ("colon", ": "),
        ("dash", " - "),
    ];
    TABLE
        .iter()
        .map(|(say, becomes)| SpokenRule {
            say: say.to_string(),
            becomes: becomes.to_string(),
        })
        .collect()
}

fn default_formatting() -> Vec<SpokenRule> {
    let mut rules = vec![SpokenRule {
        say: "at sign".to_string(),
        becomes: "@".to_string(),
    }];
    for domain in [
        "com", "org", "net", "io", "dev", "ai", "co", "app", "uk", "de",
    ] {
        rules.push(SpokenRule {
            say: format!("dot {domain}"),
            becomes: format!(".{domain}"),
        });
    }
    rules
}

fn default_commands() -> Vec<CommandRule> {
    const TABLE: [(&str, &str); 2] = [
        (
            "scratch that, cancel that, discard that, never mind, nevermind",
            "discard",
        ),
        ("quit myware, myware quit, myware stop", "quit"),
    ];
    TABLE
        .iter()
        .map(|(say, action)| CommandRule {
            say: say.to_string(),
            action: action.to_string(),
        })
        .collect()
}

fn default_shapes() -> Vec<CommandRule> {
    const TABLE: [(&str, &str); 2] = [
        ("as a list, as bullets", "bullets"),
        ("on one line, as one line", "one line"),
    ];
    TABLE
        .iter()
        .map(|(say, action)| CommandRule {
            say: say.to_string(),
            action: action.to_string(),
        })
        .collect()
}

/// What happens to the clipboard after a transcript is pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Clipboard {
    /// Put back whatever was there before the transcript. The default, because
    /// dictation is something you do in the middle of other work.
    #[default]
    Keep,
    /// Leave the transcript on the clipboard.
    Replace,
}

impl Clipboard {
    pub const ALL: [Clipboard; 2] = [Clipboard::Keep, Clipboard::Replace];

    pub fn arg(self) -> &'static str {
        match self {
            Clipboard::Keep => "keep",
            Clipboard::Replace => "replace",
        }
    }
}

impl core::fmt::Display for Clipboard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.arg())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Input device name, or `None` for the host default.
    pub device: Option<String>,
    /// How the transcript is delivered.
    pub paste: PasteMethod,
    /// Whether the clipboard goes back to what it held before the transcript.
    pub clipboard: Clipboard,
    /// Split a recording at silences into chunks this long. Parakeet degrades on
    /// long single clips and grows memory, so this is a correctness limit.
    pub chunk_seconds: f32,
    /// ONNX Runtime threads for the recognizer. Four saturates the machine this
    /// was measured on; more mostly adds contention with the audio callback.
    pub threads: i32,
    /// Drop the warm recognizer after this many minutes idle. Zero keeps it.
    pub idle_unload_minutes: u32,
    /// Append a space to each transcript so consecutive dictations concatenate.
    pub trailing_space: bool,
    /// Hard stop for one capture, in seconds.
    pub max_seconds: f32,
    /// Play a tone when recording starts, when the stop press is accepted, and
    /// when a transcript has been delivered.
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
    /// not in use. Streaming waits for a pause near this length before
    /// committing, so a word is not cut in two. The word-by-word model ignores
    /// it.
    pub stream_seconds: f32,
    /// Opt in to the small streaming zipformer for streaming dictation.
    ///
    /// Off by default: Parakeet is the accurate, multilingual model, and phrase
    /// chunking keeps streaming entirely on it. On, streaming uses the online
    /// model when its package is installed and falls back to chunks otherwise.
    pub stream_online: bool,
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
    /// Strip fillers and repeated words, and tidy the spacing, before delivery.
    pub cleanup: bool,
    /// Turn spoken "at sign" and "dot com" into addresses.
    pub formatting: bool,
    /// Treat spoken punctuation names as punctuation. Off by default: people say
    /// "comma" and "period" in prose often enough that this has to be asked for.
    pub punctuation_commands: bool,
    /// Whole-utterance commands: "scratch that", "quit myware".
    pub commands: bool,
    /// Words removed when cleanup is on.
    #[serde(default = "default_fillers")]
    pub fillers: Vec<String>,
    /// Spoken punctuation, used when `punctuation_commands` is on.
    #[serde(default = "default_punctuation")]
    pub punctuation: Vec<SpokenRule>,
    /// Spoken addresses, used when `formatting` is on.
    #[serde(default = "default_formatting")]
    pub formatting_rules: Vec<SpokenRule>,
    /// Whole-utterance instructions, used when `commands` is on.
    #[serde(default = "default_commands")]
    pub command_rules: Vec<CommandRule>,
    /// Trailing instructions that reshape the text, used when `commands` is on.
    #[serde(default = "default_shapes")]
    pub shape_rules: Vec<CommandRule>,
    /// Phrases that expand to fixed text.
    pub snippets: Vec<Snippet>,
    /// Whole-word substitutions, applied last.
    pub replacements: Vec<Replacement>,
    /// Base URL of a local Ollama server for LLM work.
    ///
    /// Loopback only: the client refuses any host that is not this machine, so
    /// this is a port and a scheme rather than an escape hatch to a cloud
    /// service. The app makes no other network call after the model download.
    pub ollama_url: String,
    /// Default model for the Ollama commands. Empty means the caller must name
    /// one; nothing is chosen silently.
    pub ollama_model: String,
    /// Seconds to wait for an Ollama response before abandoning the request.
    pub ollama_timeout_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: None,
            paste: PasteMethod::Auto,
            clipboard: Clipboard::Keep,
            chunk_seconds: 20.0,
            threads: 4,
            idle_unload_minutes: 30,
            trailing_space: true,
            max_seconds: 300.0,
            tones: true,
            hotkey: "ctrl+space".to_string(),
            stream_hotkey: "ctrl+shift+space".to_string(),
            stream_seconds: 3.0,
            stream_segment_seconds: 8.0,
            stream_online: false,
            history: false,
            history_days: 30,
            cleanup: true,
            formatting: true,
            fillers: default_fillers(),
            punctuation: default_punctuation(),
            formatting_rules: default_formatting(),
            command_rules: default_commands(),
            shape_rules: default_shapes(),
            punctuation_commands: false,
            commands: true,
            snippets: Vec::new(),
            replacements: Vec::new(),
            ollama_url: "http://127.0.0.1:11434".to_string(),
            ollama_model: String::new(),
            ollama_timeout_seconds: 30,
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

/// A cheap "has this file changed" marker.
///
/// The daemon polls this on its tick rather than watching the file: one `stat`
/// is cheaper than an inotify watch that has to be torn down and rebuilt every
/// time a settings window rewrites the file with a fresh inode. The length is
/// carried alongside the mtime because a filesystem with one-second timestamps
/// would otherwise miss an edit that did not change the file's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    /// `None` when the file is not there.
    ///
    /// That is a state worth noticing rather than an error: deleting
    /// `config.toml` is how a user gets the defaults back, and it has to take
    /// effect like any other edit.
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
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
        assert_eq!(back.ollama_url, config.ollama_url);
        assert_eq!(back.ollama_timeout_seconds, config.ollama_timeout_seconds);
        assert_eq!(PasteMethod::from_arg("ydotool"), Some(PasteMethod::Ydotool));
    }

    #[test]
    fn a_partial_file_keeps_the_other_defaults() {
        let parsed: Config = toml::from_str("paste = \"none\"\n").expect("parse");
        assert_eq!(parsed.paste, PasteMethod::None);
        assert_eq!(parsed.chunk_seconds, Config::default().chunk_seconds);
    }

    #[test]
    fn a_stamp_notices_a_rewrite_and_a_deletion() {
        let path = std::env::temp_dir().join(format!("myware-voice-stamp-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(Stamp::of(&path), None, "a missing file has no stamp");

        std::fs::write(&path, "paste = \"none\"\n").expect("write");
        let first = Stamp::of(&path).expect("stamp");
        // A different length, so this cannot pass on an mtime alone: filesystems
        // with one-second timestamps are exactly why the length is carried.
        std::fs::write(&path, "paste = \"ydotool\"\n").expect("rewrite");
        assert_ne!(Stamp::of(&path).expect("stamp"), first);

        std::fs::remove_file(&path).expect("remove");
        assert_eq!(
            Stamp::of(&path),
            None,
            "deleting the file is how the defaults come back"
        );
    }
}
