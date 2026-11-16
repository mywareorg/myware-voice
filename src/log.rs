//! The state directory: a capped log, and an opt-in transcript history.
//!
//! A headless daemon whose stderr goes to the compositor's log is undebuggable,
//! so everything worth knowing is written here. History is off by default: the
//! whole point of the app is that transcripts stay put, and keeping them at rest
//! is a decision the user should make deliberately.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Log size after which the file is truncated. A dictation daemon should not be
/// able to fill a disk by logging.
const MAX_LOG_BYTES: u64 = 1 << 20;
/// The error file is for a tester to send back, so it stays small.
const MAX_ERROR_BYTES: u64 = 256 * 1024;

pub fn dir() -> Result<PathBuf> {
    crate::dirs::state_dir()
}

pub fn path() -> Result<PathBuf> {
    crate::dirs::log_file()
}

pub fn history_path() -> Result<PathBuf> {
    crate::dirs::history_file()
}

fn ensure_dir() -> Result<()> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(())
}

/// Timestamp as `YYYY-MM-DD HH:MM`, in UTC.
pub fn stamp(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let seconds = epoch % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60
    )
}

/// Howard Hinnant's days-to-civil conversion, which is exact for any date this
/// app will ever print.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { y + 1 } else { y }, month, day)
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append one line to the log, truncating it when it grows past the cap.
pub fn write(message: &str) {
    let Ok(path) = path() else { return };
    if ensure_dir().is_err() {
        return;
    }
    if std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) > MAX_LOG_BYTES {
        rotate(&path);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "[{}] {message}", stamp(now()));
    }
}

/// The error file in the checkout root, when this build is running from one.
///
/// A tester on Windows runs the daemon from a clone; `CARGO_MANIFEST_DIR` is
/// that clone on the build machine, which is also the checkout on the test
/// machine for a local `cargo run`. An installed binary points at a path that
/// no longer exists and correctly falls back to the normal log only.
pub fn error_file() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    root.is_dir().then(|| root.join("myware-voice-error.log"))
}

/// Append an error to the log, and to the checkout-root error file.
///
/// This is the file to send back when Windows misbehaves: it is plain text, it
/// sits beside the source the tester is running, and it carries the version and
/// platform on its first line so a report does not need to be asked for them.
pub fn error(message: &str) {
    write(message);

    let Some(path) = error_file() else { return };
    let mut fresh = !path.exists();
    if std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) > MAX_ERROR_BYTES {
        rotate(&path);
        fresh = true;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    if fresh {
        let exe = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "unknown executable".to_string());
        let _ = writeln!(
            file,
            "myware-voice {} · {} {} · {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            exe
        );
    }
    let _ = writeln!(file, "[{}] {message}", stamp(now()));
}

/// Record panics in the log, keeping the default hook's stderr output.
///
/// A release build on Windows has no console, so a panic that is not written down
/// leaves nothing to report. The default hook still runs: `RUST_BACKTRACE` is
/// what produces the useful detail, and this only guarantees a line survives.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error(&format!("panic: {info}"));
        previous(info);
    }));
}

/// Move a full log aside instead of emptying it.
///
/// Truncating throws away exactly the lines a bug report needs — everything
/// before the crash. One generation is kept (`log.1`), so a runaway daemon still
/// cannot fill a disk while the evidence survives.
fn rotate(path: &Path) {
    let _ = std::fs::rename(path, rotated(path));
}

/// `log` becomes `log.1`; `myware-voice-error.log` becomes
/// `myware-voice-error.log.1`, which is what someone looking for it would guess.
fn rotated(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".1");
    path.with_file_name(name)
}

/// The last `count` lines of the log.
pub fn tail(count: usize) -> Result<Vec<String>> {
    let path = path()?;
    let mut text = String::new();
    match std::fs::File::open(&path) {
        Ok(mut file) => {
            let len = file.metadata()?.len();
            let from = len.saturating_sub(64 * 1024);
            file.seek(SeekFrom::Start(from))?;
            file.read_to_string(&mut text)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("read log"),
    }
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.len() > count {
        lines.drain(..lines.len() - count);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_log_is_rotated_not_emptied() {
        let root =
            std::env::temp_dir().join(format!("myware-voice-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("log");
        std::fs::write(&path, "the line before the crash\n").unwrap();

        rotate(&path);

        assert!(!path.exists(), "the live log was not moved aside");
        let kept = std::fs::read_to_string(rotated(&path)).expect("the rotated log");
        assert_eq!(kept, "the line before the crash\n");
        assert_eq!(rotated(&path), root.join("log.1"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_civil_conversion_matches_known_dates() {
        assert_eq!(stamp(0), "1970-01-01 00:00");
        assert_eq!(stamp(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(stamp(1_800_000_000), "2027-01-15 08:00");
    }
}
