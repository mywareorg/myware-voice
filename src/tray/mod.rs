//! The tray icon: a way to reach the settings window, the history and Quit
//! without a terminal.
//!
//! Two implementations because the platforms disagree about everything except
//! the menu: a notification-area icon on Windows, and a StatusNotifierItem on
//! Linux, which is what waybar and every GTK shell host.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
