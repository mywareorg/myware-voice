//! Getting the transcript into the focused window.
//!
//! The transcript is copied, then a paste chord is sent. Typing it out would be
//! hundreds of virtual-keyboard events, would race the user's own typing, and
//! would split across windows if focus moved mid-sentence.
//!
//! Never send Enter: a trailing newline pasted into a terminal executes a
//! command. Both backends trim it.

use std::path::PathBuf;

/// Locate an executable on `PATH`.
///
/// This replaces `sh -c command -v`, which silently answered "nothing" on
/// Windows, and it is what `doctor` reports instead of guessing.
pub fn find(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_extension = dir.join(format!("{bin}.exe"));
            if with_extension.is_file() {
                return Some(with_extension);
            }
        }
    }
    None
}

pub fn have(bin: &str) -> bool {
    find(bin).is_some()
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
