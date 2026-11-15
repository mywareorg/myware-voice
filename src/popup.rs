//! The listening popup on Linux: a layer-shell surface the daemon owns.
//!
//! The daemon starts `hippo-dictate-overlay` when a capture starts, writes one
//! JSON line per tick on its stdin, and drops the pipe on stop, which is the
//! child's shutdown signal. The process boundary is deliberate: the overlay links
//! a Wayland client and a software renderer, and none of that belongs in the
//! daemon's dependency graph or its memory while it is idle.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};

/// The running overlay, if there is a compositor that can host one.
pub struct Popup {
    child: Child,
    stdin: ChildStdin,
}

impl Popup {
    /// Start the overlay. `None` when the binary is not installed beside us or on
    /// `PATH`, which is a missing convenience rather than a failure.
    pub fn start() -> Option<Self> {
        let binary = binary()?;
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            // Inherited, not discarded: the daemon's stderr is the log file, so a
            // compositor that cannot host a layer surface leaves its reason there
            // instead of the popup simply never appearing.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| crate::log::error(&format!("popup: {error}")))
            .ok()?;
        let stdin = child.stdin.take()?;
        Some(Self { child, stdin })
    }

    /// One tick: the input level, and whether phrases are being committed. A
    /// closed pipe means the overlay died, which is not worth reporting ten times
    /// a second.
    pub fn send(&mut self, status: &crate::daemon::Status) {
        let line = serde_json::json!({
            "level": status.level,
            "streaming": status.streaming,
        });
        let _ = writeln!(self.stdin, "{line}");
    }

    /// Close the pipe and reap the child: on EOF it exits by itself, and the kill
    /// covers a compositor that left it blocked.
    pub fn stop(mut self) {
        drop(self.stdin);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn binary() -> Option<std::path::PathBuf> {
    let sibling = std::env::current_exe()
        .ok()?
        .parent()?
        .join("hippo-dictate-overlay");
    if sibling.is_file() {
        return Some(sibling);
    }
    crate::inject::find("hippo-dictate-overlay")
}
