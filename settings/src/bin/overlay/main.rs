//! The listening popup.
//!
//! Linux only: it is a `wlr-layer-shell` surface, and the Windows popup is drawn
//! inside the daemon instead (`src/overlay.rs`). The binary still exists on other
//! targets so that `cargo build --workspace` does not have to be told which
//! platform it is on — the whole implementation is behind the `cfg`.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

/// Nothing to do: the daemon never starts this process off Linux.
#[cfg(not(target_os = "linux"))]
fn main() {}
