//! The state directory: a capped log, and an opt-in transcript history.
//!
//! A headless daemon whose stderr goes to the compositor's log is undebuggable,
//! so everything worth knowing is written here. History is off by default: the
//! whole point of the app is that transcripts stay put, and keeping them at rest
//! is a decision the user should make deliberately.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Log size after which the file is truncated. A dictation daemon should not be
/// able to fill a disk by logging.
const MAX_LOG_BYTES: u64 = 1 << 20;

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
        let _ = std::fs::write(&path, b"");
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "[{}] {message}", stamp(now()));
    }
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

/// Append a delivered transcript. `1` is a tab: the text may contain anything.
pub fn append_history(text: &str) -> Result<()> {
    ensure_dir()?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path()?)?;
    writeln!(file, "{}\t{}", now(), text.replace(['\n', '\r'], " "))?;
    Ok(())
}

/// Lines from the last `days` days, oldest first.
pub fn recent_history(days: u32) -> Result<Vec<String>> {
    let path = history_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("read history"),
    };
    let cutoff = now().saturating_sub(days.max(1) as u64 * 86_400);
    Ok(text
        .lines()
        .filter_map(|line| {
            let (epoch, body) = line.split_once('\t')?;
            let epoch: u64 = epoch.parse().ok()?;
            (epoch >= cutoff).then(|| format!("{}  {body}", stamp(epoch)))
        })
        .collect())
}

/// Drop entries older than `days`. Called after each append so the file cannot
/// grow without bound.
pub fn prune_history(days: u32) -> Result<()> {
    let path = history_path()?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let cutoff = now().saturating_sub(days.max(1) as u64 * 86_400);
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            line.split_once('\t')
                .and_then(|(epoch, _)| epoch.parse::<u64>().ok())
                .map(|epoch| epoch >= cutoff)
                .unwrap_or(false)
        })
        .collect();
    if kept.len() != text.lines().count() {
        std::fs::write(&path, kept.join("\n") + "\n")?;
    }
    Ok(())
}

pub fn clear_history() -> Result<()> {
    let path = history_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove history"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_civil_conversion_matches_known_dates() {
        assert_eq!(stamp(0), "1970-01-01 00:00");
        assert_eq!(stamp(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(stamp(1_800_000_000), "2027-01-15 08:00");
    }
}
