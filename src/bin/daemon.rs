//! The daemon as its own binary.
//!
//! It exists for one reason: a release build on Windows can be a GUI-subsystem
//! executable, so start-on-login leaves no console window behind for the life of
//! the process. Debug builds keep the console subsystem, because that is where a
//! startup failure can actually be read; the log catches the release case.
//! `hippo-voice daemon` still works and is what a development build uses.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
    match hippo_voice::daemon::run_process() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // A release binary has no console, so stderr usually goes nowhere.
            // The log is the only record of why the process exited.
            let message = format!("daemon: startup failed: {error:#}");
            hippo_voice::log::error(&message);
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
