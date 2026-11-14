//! Which model package this application uses, and where it lives.
//!
//! The integrity checks and the transfer are `hippo_sherpa::model`; what is here
//! is the part that belongs to this app: the two embedded locks, the install
//! location, and the override variables `doctor` reports. A second application
//! has its own copy of this file with its own locks, and they share nothing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use hippo_sherpa::model::{sha256_of, Lock, LockedFile, State};

const LOCK: &str = include_str!("../models.lock");
const STREAMING_LOCK: &str = include_str!("../models-streaming.lock");

pub fn lock() -> Result<Lock> {
    Lock::parse(LOCK).context("parse the embedded models.lock")
}

pub fn streaming_lock() -> Result<Lock> {
    Lock::parse(STREAMING_LOCK).context("parse the embedded models-streaming.lock")
}

/// Where a model package is installed for this user.
///
/// `$XDG_DATA_HOME` (or `~/.local/share`) rather than the runtime directory: the
/// model is a 640 MB artifact that must survive logout and reboot.
pub fn install_root() -> Result<PathBuf> {
    crate::dirs::model_dir()
}

/// The directory the batch model should load from.
pub fn resolve_dir() -> Result<PathBuf> {
    resolve_locked(&lock()?, "HIPPO_VOICE_MODEL_DIR")
}

/// The directory the streaming model should load from.
pub fn streaming_dir() -> Result<PathBuf> {
    resolve_locked(&streaming_lock()?, "HIPPO_VOICE_STREAMING_MODEL_DIR")
}

fn resolve_locked(lock: &Lock, variable: &str) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(variable) {
        return Ok(PathBuf::from(dir));
    }
    let installed = install_root()?.join(&lock.dir);
    if installed.is_dir() {
        return Ok(installed);
    }
    let dev = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("vendor/models")
        .join(&lock.dir);
    if dev.is_dir() {
        return Ok(dev);
    }
    Ok(installed)
}

pub fn inspect(dir: &Path, hash: bool) -> Result<Vec<(LockedFile, State, u64)>> {
    hippo_sherpa::model::inspect(&lock()?, dir, hash)
}

pub fn inspect_streaming(dir: &Path, hash: bool) -> Result<Vec<(LockedFile, State, u64)>> {
    hippo_sherpa::model::inspect(&streaming_lock()?, dir, hash)
}

pub fn verify(dir: &Path) -> Result<bool> {
    hippo_sherpa::model::verify(&lock()?, dir)
}

pub fn verify_streaming(dir: &Path) -> Result<bool> {
    hippo_sherpa::model::verify(&streaming_lock()?, dir)
}

/// Why this directory cannot be used as a model, if it cannot. The diagnosis is
/// the library's; the command that fixes it is this application's.
pub fn unusable_reason(dir: &Path) -> Option<String> {
    let lock = lock().ok()?;
    hippo_sherpa::model::unusable_reason(&lock, dir)
        .map(|reason| format!("{reason}; run `hippo-voice model download`"))
}

pub fn download(dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<(usize, u64)> {
    hippo_sherpa::model::download(&lock()?, dir, progress)
}

pub fn download_streaming(dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<(usize, u64)> {
    hippo_sherpa::model::download(&streaming_lock()?, dir, progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_parses_and_describes_what_the_app_loads() {
        let lock = lock().expect("embedded lock");
        assert_eq!(lock.file.len(), 4);
        assert!(lock.dir.starts_with("parakeet"));
        assert!(lock.base.starts_with("https://"));
        assert!(lock.license.contains("CC-BY"));
        for file in &lock.file {
            assert_eq!(file.sha256.len(), 64, "{} has a short hash", file.name);
        }
    }

    #[test]
    fn the_streaming_lock_describes_the_smaller_package() {
        let lock = streaming_lock().expect("embedded streaming lock");
        assert_eq!(lock.file.len(), 4);
        assert_eq!(lock.dir, "streaming-zipformer-en-int8");
        assert!(lock.license.contains("Apache"));
        for file in &lock.file {
            assert_eq!(file.sha256.len(), 64, "{} has a short hash", file.name);
        }
    }

    #[test]
    fn a_missing_file_is_reported_missing_rather_than_corrupt() {
        let dir = std::env::temp_dir().join(format!("hippo-voice-model-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rows = inspect(&dir, false).unwrap();
        assert!(rows.iter().all(|(_, state, _)| *state == State::Missing));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
