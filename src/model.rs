//! The Parakeet package: where it lives, whether it is intact, and getting it.
//!
//! The lock file is compiled in, so a binary always knows exactly which files it
//! expects and what they must hash to. A truncated ONNX graph otherwise fails
//! deep inside onnxruntime with a message that says nothing about the cause.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const LOCK: &str = include_str!("../models.lock");
const STREAMING_LOCK: &str = include_str!("../models-streaming.lock");

#[derive(Debug, Deserialize)]
pub struct Lock {
    pub version: String,
    pub dir: String,
    pub base: String,
    pub license: String,
    pub attribution: String,
    pub file: Vec<LockedFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LockedFile {
    pub name: String,
    pub sha256: String,
}

pub fn lock() -> Result<Lock> {
    toml::from_str(LOCK).context("parse the embedded models.lock")
}

pub fn streaming_lock() -> Result<Lock> {
    toml::from_str(STREAMING_LOCK).context("parse the embedded models-streaming.lock")
}

/// Where a model package is installed for this user.
///
/// `$XDG_DATA_HOME` (or `~/.local/share`) rather than the runtime directory: the
/// model is a 640 MB artifact that must survive logout and reboot.
pub fn install_root() -> Result<PathBuf> {
    crate::dirs::model_dir()
}

/// The directory the batch model should load from.
///
/// Installed packages win over the source tree, so a developer who has both does
/// not silently run a stale vendored copy.
pub fn resolve_dir() -> Result<PathBuf> {
    resolve_locked(&lock()?, "HIPPO_DICTATE_MODEL_DIR")
}

/// The directory the streaming model should load from.
pub fn streaming_dir() -> Result<PathBuf> {
    resolve_locked(&streaming_lock()?, "HIPPO_DICTATE_STREAMING_MODEL_DIR")
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Present,
    Missing,
    Corrupt,
}

impl State {
    pub fn arg(self) -> &'static str {
        match self {
            State::Present => "present",
            State::Missing => "missing",
            State::Corrupt => "corrupt",
        }
    }
}

/// Check every locked file. Hashing reads the whole package, so `status` only
/// looks for the files and `verify` hashes them.
pub fn inspect(dir: &Path, hash: bool) -> Result<Vec<(LockedFile, State, u64)>> {
    inspect_with(&lock()?, dir, hash)
}

pub fn inspect_streaming(dir: &Path, hash: bool) -> Result<Vec<(LockedFile, State, u64)>> {
    inspect_with(&streaming_lock()?, dir, hash)
}

fn inspect_with(lock: &Lock, dir: &Path, hash: bool) -> Result<Vec<(LockedFile, State, u64)>> {
    let mut rows = Vec::new();
    for file in &lock.file {
        let path = dir.join(&file.name);
        let state = match std::fs::metadata(&path) {
            Err(_) => State::Missing,
            Ok(_) if !hash => State::Present,
            Ok(_) => {
                if sha256_of(&path)? == file.sha256 {
                    State::Present
                } else {
                    State::Corrupt
                }
            }
        };
        let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        rows.push((file.clone(), state, bytes));
    }
    Ok(rows)
}

/// True when every file is present and hashes correctly.
pub fn verify(dir: &Path) -> Result<bool> {
    Ok(inspect(dir, true)?
        .into_iter()
        .all(|(_, state, _)| state == State::Present))
}

pub fn verify_streaming(dir: &Path) -> Result<bool> {
    Ok(inspect_streaming(dir, true)?
        .into_iter()
        .all(|(_, state, _)| state == State::Present))
}

/// Why this directory cannot be used as a model, if it cannot.
///
/// Presence only, never a hash: this runs at daemon startup and must not read
/// 640 MB before the first press. The point is to say so before the first
/// utterance rather than after it, when the silence looks like a broken app.
pub fn unusable_reason(dir: &Path) -> Option<String> {
    let rows = match inspect(dir, false) {
        Ok(rows) => rows,
        Err(error) => return Some(format!("{error:#}")),
    };
    let missing: Vec<&str> = rows
        .iter()
        .filter(|(_, state, _)| *state != State::Present)
        .map(|(file, _, _)| file.name.as_str())
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "no usable model in {} (missing {}); run `hippo-dictate model download`",
            dir.display(),
            missing.join(", ")
        ))
    }
}

pub fn sha256_of(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Download every missing or corrupt file, resuming partial ones.
///
/// `curl` does the transfer rather than a Rust HTTP client: it resumes with
/// `--continue-at -`, it is present on Windows 10 and every Linux we target, and
/// it keeps a TLS stack and its C build out of the dependency tree. The checksum
/// is ours either way.
pub fn download(dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<(usize, u64)> {
    download_with(&lock()?, dir, progress)
}

pub fn download_streaming(dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<(usize, u64)> {
    download_with(&streaming_lock()?, dir, progress)
}

fn download_with(lock: &Lock, dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<(usize, u64)> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let mut fetched = 0usize;
    let mut total_bytes = 0u64;
    for file in &lock.file {
        let target = dir.join(&file.name);
        if target.is_file() && sha256_of(&target)? == file.sha256 {
            continue;
        }

        progress(&file.name);
        let part = dir.join(format!("{}.part", file.name));
        let url = format!("{}/{}", lock.base, file.name);
        let status = std::process::Command::new("curl")
            .args(["-fSL", "--retry", "3", "--continue-at", "-", "-o"])
            .arg(&part)
            .arg(&url)
            .status()
            .context("run curl")?;
        if !status.success() {
            bail!("curl exited with {status} for {}", file.name);
        }

        if sha256_of(&part)? != file.sha256 {
            bail!(
                "{} did not match its checksum; keeping {} for a retry",
                file.name,
                part.display()
            );
        }
        let bytes = std::fs::metadata(&part).map(|meta| meta.len()).unwrap_or(0);
        std::fs::rename(&part, &target)?;
        fetched += 1;
        total_bytes += bytes;
    }
    Ok((fetched, total_bytes))
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
        let dir = std::env::temp_dir().join("hippo-dictate-model-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rows = inspect(&dir, false).unwrap();
        assert!(rows.iter().all(|(_, state, _)| *state == State::Missing));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
