//! Getting the transcript into the focused window.
//!
//! The transcript is copied, then a paste chord is sent. Typing it out would be
//! hundreds of virtual-keyboard events, would race the user's own typing, and
//! would split across windows if focus moved mid-sentence.
//!
//! Never send Enter: a trailing newline pasted into a terminal executes a
//! command. Both backends trim it.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
