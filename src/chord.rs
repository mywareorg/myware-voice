//! Parsing a hotkey chord.
//!
//! The chord is written the way people say it — `ctrl+alt+space` — and only this
//! module decides what that means. Keeping it out of the platform code makes the
//! grammar testable on any host, which matters because a wrong keycode is
//! invisible until someone presses the key.

use std::fmt;

use anyhow::{bail, Result};

/// A key, resolved per platform by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Space,
    Enter,
    Tab,
    Escape,
    Backspace,
    Function(u8),
    Char(char),
}

/// Modifiers plus one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Super on Linux, Win on Windows.
    pub win: bool,
    pub key: Key,
}

impl Chord {
    /// Parse `ctrl+alt+space`. Modifiers are case-insensitive and may appear in
    /// any order; exactly one non-modifier key is required.
    pub fn parse(text: &str) -> Result<Self> {
        let mut chord = Self {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            key: Key::Space,
        };
        let mut key = None;

        for token in text.split('+').map(|part| part.trim().to_lowercase()) {
            if token.is_empty() {
                continue;
            }
            match token.as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "alt" | "option" | "opt" => chord.alt = true,
                "shift" => chord.shift = true,
                "super" | "win" | "meta" | "cmd" | "command" => chord.win = true,
                other => {
                    if key.is_some() {
                        bail!("hotkey has more than one key: {text:?}");
                    }
                    key = Some(parse_key(other)?);
                }
            }
        }

        let key = key.ok_or_else(|| anyhow::anyhow!("hotkey has no key: {text:?}"))?;
        if !(chord.ctrl || chord.alt || chord.shift || chord.win) {
            bail!("hotkey needs at least one modifier: {text:?}");
        }
        chord.key = key;
        Ok(chord)
    }
}

fn parse_key(token: &str) -> Result<Key> {
    Ok(match token {
        "space" | "spacebar" => Key::Space,
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        name if name.len() >= 2 && name.starts_with('f') => {
            let number: u8 = name[1..]
                .parse()
                .map_err(|_| anyhow::anyhow!("unknown key {name:?}"))?;
            if !(1..=24).contains(&number) {
                bail!("function key out of range: {name:?}");
            }
            Key::Function(number)
        }
        single if single.chars().count() == 1 => Key::Char(single.chars().next().unwrap()),
        other => bail!("unknown key {other:?}"),
    })
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("ctrl".to_string());
        }
        if self.alt {
            parts.push("alt".to_string());
        }
        if self.shift {
            parts.push("shift".to_string());
        }
        if self.win {
            parts.push("super".to_string());
        }
        parts.push(match self.key {
            Key::Space => "space".to_string(),
            Key::Enter => "enter".to_string(),
            Key::Tab => "tab".to_string(),
            Key::Escape => "escape".to_string(),
            Key::Backspace => "backspace".to_string(),
            Key::Function(number) => format!("f{number}"),
            Key::Char(character) => character.to_string(),
        });
        f.write_str(&parts.join("+"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_default_parses_and_round_trips() {
        let chord = Chord::parse(&crate::config::Config::default().hotkey).expect("parse");
        assert!(chord.ctrl);
        assert!(!chord.alt && !chord.shift && !chord.win);
        assert_eq!(chord.key, Key::Space);
        assert_eq!(chord.to_string(), "ctrl+space");
    }

    #[test]
    fn the_streaming_default_adds_shift() {
        let chord = Chord::parse(&crate::config::Config::default().stream_hotkey).expect("parse");
        assert!(chord.ctrl && chord.shift);
        assert!(!chord.alt && !chord.win);
        assert_eq!(chord.to_string(), "ctrl+shift+space");
    }

    #[test]
    fn order_and_case_do_not_matter() {
        let a = Chord::parse("Alt+Control+SPACE").expect("parse");
        let b = Chord::parse("ctrl+alt+space").expect("parse");
        assert_eq!(a, b);
    }

    #[test]
    fn function_keys_and_letters_are_understood() {
        assert_eq!(Chord::parse("ctrl+f9").unwrap().key, Key::Function(9));
        assert_eq!(Chord::parse("super+d").unwrap().key, Key::Char('d'));
    }

    #[test]
    fn a_chord_needs_a_modifier_and_exactly_one_key() {
        assert!(Chord::parse("space").is_err(), "no modifier");
        assert!(Chord::parse("ctrl").is_err(), "no key");
        assert!(Chord::parse("ctrl+a+b").is_err(), "two keys");
        assert!(Chord::parse("ctrl+banana").is_err(), "unknown key");
    }
}
