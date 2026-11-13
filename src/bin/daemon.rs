//! The daemon as its own binary.
//!
//! It exists for one reason: on Windows this can be built as a GUI-subsystem
//! executable, so start-on-login leaves no console window behind for the life of
//! the process. `hippo-voice daemon` still works and is what a development build
//! uses.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    hippo_voice::daemon::run_process()
}
