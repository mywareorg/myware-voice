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

/// What the focused window looks like, as far as the compositor will say.
#[derive(Debug, Clone, Eq)]
pub struct Focus {
    /// Application id on sway/Wayland, class on X11-flavoured trees.
    pub app_id: String,
    /// The window title. Kept for logs and diagnostics only: it is not part of a
    /// window's identity.
    pub title: String,
    /// Provider-defined window identity: the sway node id, or the Hyprland
    /// address. Two windows of the same application differ here.
    pub window_id: Option<String>,
}

impl PartialEq for Focus {
    /// Identity is the window, not its contents.
    ///
    /// Titles change constantly for reasons the user cannot see: a shell prompt
    /// after every command, an editor's dirty marker on the first keystroke. When
    /// they were compared, a normal pause between pressing stop and the paste
    /// arriving looked like the user had switched windows, so the paste was
    /// refused. Comparing the window itself is what "did focus move" means.
    fn eq(&self, other: &Self) -> bool {
        self.app_id == other.app_id && self.window_id == other.window_id
    }
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

/// What is on the clipboard now, if it is text.
///
/// Read before a transcript is written, so it can be put back: dictation happens
/// in the middle of other work, and the thing you had copied is usually still
/// wanted. `None` covers both "no clipboard tool" and "not text", which the caller
/// treats the same way — nothing to restore.
pub fn read_clipboard() -> Option<String> {
    if !have("wl-paste") {
        return None;
    }
    let output = Command::new("wl-paste")
        .arg("--no-newline")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Put `text` back on the clipboard exactly as it was.
///
/// Not `copy`: that trims trailing newlines because a transcript must never end
/// in one. A restored selection is the user's own bytes and is returned intact.
pub fn set_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
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

/// Which compositor can answer "what is focused".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Session {
    Sway,
    Hyprland,
    /// X11, or a Wayland compositor with no focus protocol the daemon can read.
    Unknown,
}

/// Identify the session from the variables the compositor exports.
///
/// The variable, not the binary: `swaymsg` can be installed on a Hyprland
/// machine and would answer with its own error, and probing a socket that is not
/// there costs a process spawn every time a capture ends.
fn session() -> Session {
    if std::env::var_os("SWAYSOCK").is_some() {
        Session::Sway
    } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        Session::Hyprland
    } else {
        Session::Unknown
    }
}

/// Why a transcript could not be delivered at all, if it could not.
///
/// Every delivery path starts with `wl-copy`, including the copy-only one, so
/// without it there is nothing to paste and nothing to fall back to. The daemon
/// asks before it records: refusing up front is honest, and it means the user is
/// not left talking into a microphone that produces nothing.
pub fn unavailable() -> Option<String> {
    // The fake-delivery seam skips the clipboard and the paste entirely, which is
    // how the state machine is tested on a machine that has neither. Asking for
    // tools that run would never be used would fail every such test.
    if crate::compat::var_os("FAKE_DELIVERY").is_some() {
        return None;
    }
    if have("wl-copy") {
        None
    } else {
        Some(
            "wl-copy is not installed; it is how a transcript leaves the daemon \
             (install wl-clipboard)"
                .to_string(),
        )
    }
}

/// The focused window, from whichever compositor can say.
pub fn focus() -> Option<Focus> {
    if let Some(spec) = crate::compat::var_os("FAKE_FOCUS") {
        return Some(parse_fake(&spec.to_string_lossy()));
    }
    match session() {
        Session::Sway => sway_focus(),
        Session::Hyprland => hyprland_focus(),
        Session::Unknown => None,
    }
}

/// `app_id:window_id`, the form `MYWARE_VOICE_FAKE_FOCUS` takes. A spec without a
/// colon has no window id, which is the "provider that only knows the
/// application" case.
fn parse_fake(spec: &str) -> Focus {
    let (app_id, window_id) = match spec.split_once(':') {
        Some((app_id, window_id)) => (app_id, Some(window_id.to_string())),
        None => (spec, None),
    };
    Focus {
        app_id: app_id.to_string(),
        title: String::new(),
        window_id,
    }
}

/// The focused window according to Hyprland.
fn hyprland_focus() -> Option<Focus> {
    let output = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let window: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    // An empty object means nothing is focused, which is not an error.
    let app_id = window.get("class").and_then(|value| value.as_str())?;
    Some(Focus {
        app_id: app_id.to_string(),
        title: window
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        window_id: window
            .get("address")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    })
}

/// The focused window according to sway.
fn sway_focus() -> Option<Focus> {
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
    let window_id = node
        .get("id")
        .and_then(|value| value.as_u64())
        .map(|id| id.to_string());
    Some(Focus {
        app_id,
        title,
        window_id,
    })
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
        .args(["-a", "myware-voice", summary, body])
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
            window_id: Some("1".into()),
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
            window_id: Some("2".into()),
        };
        assert!(!focus.is_terminal());
    }

    #[test]
    fn the_fake_focus_spec_carries_the_window_identity() {
        let focus = parse_fake("foot:7");
        assert_eq!(focus.app_id, "foot");
        assert_eq!(focus.window_id.as_deref(), Some("7"));

        // No colon: a provider that knows the application but not the window.
        let vague = parse_fake("foot");
        assert_eq!(vague.app_id, "foot");
        assert_eq!(vague.window_id, None);
        assert_ne!(vague, parse_fake("foot:1"));
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
