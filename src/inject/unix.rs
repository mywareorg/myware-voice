//! Getting the transcript into the focused window.
//!
//! The transcript is copied, then a paste chord is sent. Typing it out would be
//! hundreds of virtual-keyboard events, would race the user's own typing, and
//! would split across windows if focus moved mid-sentence.
//!
//! Never send Enter: a trailing newline pasted into a terminal executes a
//! command. The clipboards are trimmed for that reason.

use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use super::have;
use crate::config::PasteMethod;

/// What the focused window looks like, as far as sway will say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    pub app_id: String,
    pub title: String,
}

impl Focus {
    /// Terminals need `ctrl+shift+v`; a bare `ctrl+v` inserts a literal `^V`.
    pub fn is_terminal(&self) -> bool {
        const TERMINALS: &[&str] = &[
            "foot",
            "alacritty",
            "kitty",
            "wezterm",
            "ghostty",
            "org.gnome.console",
            "org.gnome.terminal",
            "konsole",
            "xterm",
            "st",
            "urxvt",
            "tilix",
            "terminator",
        ];
        let id = self.app_id.to_lowercase();
        TERMINALS
            .iter()
            .any(|name| id == *name || id.contains(name))
    }
}

/// The paste chord for a method, as argv.
pub fn paste_argv(method: PasteMethod, terminal: bool) -> Option<Vec<String>> {
    let argv: Vec<&str> = match (method, terminal) {
        (PasteMethod::None, _) => return None,
        (PasteMethod::Wtype, false) => vec!["wtype", "-M", "ctrl", "v", "-m", "ctrl"],
        (PasteMethod::Wtype, true) => vec![
            "wtype", "-M", "ctrl", "-M", "shift", "v", "-m", "shift", "-m", "ctrl",
        ],
        // 29 = ctrl, 42 = shift, 47 = v. The `:1` is press and `:0` release.
        (PasteMethod::Ydotool, false) => vec!["ydotool", "key", "29:1", "47:1", "47:0", "29:0"],
        (PasteMethod::Ydotool, true) => vec![
            "ydotool", "key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0",
        ],
        // `Auto` is resolved before this is called.
        (PasteMethod::Auto, _) => return None,
    };
    Some(argv.into_iter().map(str::to_string).collect())
}

/// Which method will actually work here.
pub fn resolve(method: PasteMethod) -> PasteMethod {
    match method {
        PasteMethod::Auto => {
            if have("wtype") {
                PasteMethod::Wtype
            } else if have("ydotool") {
                PasteMethod::Ydotool
            } else {
                PasteMethod::None
            }
        }
        explicit => {
            if explicit == PasteMethod::None || have(explicit.arg()) {
                explicit
            } else {
                PasteMethod::None
            }
        }
    }
}

/// Put `text` on the Wayland clipboard, without a trailing newline.
pub fn copy(text: &str) -> Result<()> {
    let text = text.trim_end_matches(['\n', '\r']);
    let mut child = Command::new("wl-copy")
        .arg("--trim-newline")
        .stdin(Stdio::piped())
        .spawn()
        .context("spawn wl-copy")?;
    {
        use std::io::Write;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("wl-copy stdin"))?;
        stdin
            .write_all(text.as_bytes())
            .context("write to wl-copy")?;
    }
    let status = child.wait().context("wait for wl-copy")?;
    if !status.success() {
        bail!("wl-copy exited with {status}");
    }
    Ok(())
}

/// Send a paste chord. Returns the method used, or `None` when only the
/// clipboard was updated.
pub fn paste(method: PasteMethod, focus: Option<&Focus>) -> Result<PasteMethod> {
    let method = resolve(method);
    let terminal = focus.map(Focus::is_terminal).unwrap_or(false);
    let Some(argv) = paste_argv(method, terminal) else {
        return Ok(PasteMethod::None);
    };
    let (program, args) = argv.split_first().expect("argv is never empty");
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(method)
}

/// The focused window according to sway.
pub fn focus() -> Option<Focus> {
    let output = Command::new("swaymsg")
        .args(["-t", "get_tree", "--raw"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tree: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let node = find_focused(&tree)?;
    let app_id = node
        .get("app_id")
        .and_then(|value| value.as_str())
        .or_else(|| {
            node.get("window_properties")
                .and_then(|props| props.get("class"))
                .and_then(|value| value.as_str())
        })
        .unwrap_or_default()
        .to_string();
    let title = node
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    Some(Focus { app_id, title })
}

fn find_focused(node: &serde_json::Value) -> Option<&serde_json::Value> {
    if node.get("focused").and_then(|value| value.as_bool()) == Some(true) {
        return Some(node);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(key).and_then(|value| value.as_array()) {
            for child in children {
                if let Some(found) = find_focused(child) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// A desktop notification, for when the paste could not be delivered.
pub fn notify(summary: &str, body: &str) {
    if !have("notify-send") {
        return;
    }
    let _ = Command::new("notify-send")
        .args(["-a", "hippo-voice", summary, body])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_get_the_shifted_chord() {
        let term = Focus {
            app_id: "foot".into(),
            title: "~".into(),
        };
        assert!(term.is_terminal());
        let argv = paste_argv(PasteMethod::Wtype, true).unwrap();
        assert!(argv.iter().any(|arg| arg == "shift"));
        let plain = paste_argv(PasteMethod::Wtype, false).unwrap();
        assert!(!plain.iter().any(|arg| arg == "shift"));
    }

    #[test]
    fn a_browser_is_not_a_terminal() {
        let focus = Focus {
            app_id: "firefox".into(),
            title: "Inbox".into(),
        };
        assert!(!focus.is_terminal());
    }

    #[test]
    fn no_paste_method_means_no_argv() {
        assert!(paste_argv(PasteMethod::None, false).is_none());
        assert!(paste_argv(PasteMethod::Auto, false).is_none());
    }

    #[test]
    fn the_ydotool_chord_presses_and_releases_every_key() {
        let argv = paste_argv(PasteMethod::Ydotool, false).unwrap();
        assert_eq!(argv, ["ydotool", "key", "29:1", "47:1", "47:0", "29:0"]);
    }
}
