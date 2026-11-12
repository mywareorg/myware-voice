//! The Parakeet package: where it lives, whether it is intact, and getting it.
//!
//! The lock file is compiled in, so a binary always knows exactly which files it
//! expects and what they must hash to. A truncated ONNX graph otherwise fails
//! deep inside onnxruntime with a message that says nothing about the cause.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const LOCK: &str = include_str!("../models.lock");

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

/// Where a model package is installed for this user.
///
/// `$XDG_DATA_HOME` (or `~/.local/share`) rather than the runtime directory: the
/// model is a 640 MB artifact that must survive logout and reboot.
pub fn install_root() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| anyhow::anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;
    Ok(base.join("hippo-voice/models"))
}

/// The directory the app should load from.
///
/// Installed packages win over the source tree, so a developer who has both does
/// not silently run a stale vendored copy.
pub fn resolve_dir() -> Result<PathBuf> {
    let lock = lock()?;
    if let Some(dir) = std::env::var_os("HIPPO_VOICE_MODEL_DIR") {
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
    let lock = lock()?;
    let mut rows = Vec::new();
    for file in lock.file {
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
        rows.push((file, state, bytes));
    }
    Ok(rows)
}

/// True when every file is present and hashes correctly.
pub fn verify(dir: &Path) -> Result<bool> {
    Ok(inspect(dir, true)?
        .into_iter()
        .all(|(_, state, _)| state == State::Present))
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
/// Returns the number of files fetched and the bytes written.
pub fn download(dir: &Path, progress: &mut dyn FnMut(&str, u64, u64)) -> Result<(usize, u64)> {
    let lock = lock()?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let mut fetched = 0usize;
    let mut total_bytes = 0u64;
    for file in &lock.file {
        let target = dir.join(&file.name);
        if target.is_file() && sha256_of(&target)? == file.sha256 {
            continue;
        }

        let part = target.with_extension(format!(
            "{}.part",
            target.extension().and_then(|e| e.to_str()).unwrap_or("bin")
        ));
        let url = format!("{}/{}", lock.base, file.name);

        let mut already = std::fs::metadata(&part).map(|meta| meta.len()).unwrap_or(0);
        let mut request = ureq::get(&url);
        if already > 0 {
            request = request.set("Range", &format!("bytes={already}-"));
        }
        let response = request.call().with_context(|| format!("GET {url}"))?;

        // A server that ignores Range starts from zero; drop the partial file so
        // the two are not concatenated.
        if already > 0 && response.status() != 206 {
            already = 0;
        }

        let expected = response
            .header("content-length")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            + already;

        let mut reader = response.into_reader();
        let mut sink = std::fs::OpenOptions::new()
            .create(true)
            .append(already > 0)
            .write(true)
            .truncate(already == 0)
            .open(&part)
            .with_context(|| format!("open {}", part.display()))?;

        let mut written = already;
        let mut buffer = vec![0u8; 1 << 16];
        loop {
            let read = reader.read(&mut buffer).context("read response")?;
            if read == 0 {
                break;
            }
            sink.write_all(&buffer[..read])?;
            written += read as u64;
            progress(&file.name, written, expected);
        }
        sink.flush()?;
        drop(sink);

        if sha256_of(&part)? != file.sha256 {
            bail!(
                "{} did not match its checksum; keeping {} for a retry",
                file.name,
                part.display()
            );
        }
        std::fs::rename(&part, &target)?;
        fetched += 1;
        total_bytes += written - already;
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
    fn a_missing_file_is_reported_missing_rather_than_corrupt() {
        let dir = std::env::temp_dir().join("hippo-voice-model-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rows = inspect(&dir, false).unwrap();
        assert!(rows.iter().all(|(_, state, _)| *state == State::Missing));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
