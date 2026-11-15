//! Delivered transcripts kept at rest.
//!
//! Off by default: the premise of the app is that transcripts stay where the user
//! puts them, so keeping them on disk is a decision. When it is on, entries are
//! tab-separated epoch seconds and text, pruned by age after every append, and
//! read back here for the history pane and for `hippo-dictate history`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub struct Entry {
    pub epoch: u64,
    pub text: String,
}

fn now() -> u64 {
    crate::log::now()
}

pub fn path() -> Result<PathBuf> {
    crate::dirs::history_file()
}

/// Append one delivered transcript. Newlines are flattened: the file is one
/// record per line.
pub fn append(text: &str) -> Result<()> {
    use std::io::Write as _;

    crate::dirs::state_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))
    })?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path()?)?;
    writeln!(file, "{}\t{}", now(), text.replace(['\n', '\r'], " "))?;
    Ok(())
}

/// Entries from the last `days` days, oldest first.
pub fn recent(days: u32) -> Result<Vec<Entry>> {
    recent_from(&path()?, days, now())
}

/// `recent` against an explicit file and clock, so it can be tested.
pub fn recent_from(path: &Path, days: u32, now: u64) -> Result<Vec<Entry>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let cutoff = now.saturating_sub(days.max(1) as u64 * 86_400);
    Ok(text
        .lines()
        .filter_map(|line| {
            let (epoch, text) = line.split_once('\t')?;
            let epoch: u64 = epoch.parse().ok()?;
            (epoch >= cutoff).then(|| Entry {
                epoch,
                text: text.to_string(),
            })
        })
        .collect())
}

/// Entries whose text contains `needle`, case-insensitively.
///
/// A substring match rather than a word search: what people remember is a
/// fragment — a name, an invoice number — and the window is small enough that
/// scanning it is the point. An empty needle keeps everything.
pub fn filter(entries: Vec<Entry>, needle: &str) -> Vec<Entry> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|entry| entry.text.to_lowercase().contains(&needle))
        .collect()
}

/// Drop entries older than `days`.
pub fn prune(days: u32) -> Result<()> {
    let path = path()?;
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

pub fn clear() -> Result<()> {
    let path = path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hippo-dictate-history-{name}-{}.tsv",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn entries_are_read_back_with_their_timestamps() {
        let path = temp("read");
        std::fs::write(
            &path,
            "1000\tfirst line\n2000\tsecond line\n3000\tthird line\n",
        )
        .unwrap();

        let entries = recent_from(&path, 30, 3000).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].text, "first line");
        assert_eq!(entries[2].epoch, 3000);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_search_is_case_insensitive_and_keeps_order() {
        let entries = vec![
            Entry {
                epoch: 1,
                text: "send the Invoice to Ada".to_string(),
            },
            Entry {
                epoch: 2,
                text: "book a table".to_string(),
            },
            Entry {
                epoch: 3,
                text: "chase the invoice again".to_string(),
            },
        ];
        let found = filter(entries, "INVOICE");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].epoch, 1);
        assert_eq!(found[1].epoch, 3);
    }

    #[test]
    fn an_empty_search_keeps_everything() {
        let entries = vec![Entry {
            epoch: 1,
            text: "anything".to_string(),
        }];
        assert_eq!(filter(entries, "   ").len(), 1);
    }

    #[test]
    fn entries_older_than_the_retention_window_are_dropped() {
        let path = temp("window");
        let day = 86_400u64;
        std::fs::write(&path, format!("{}\told\n{}\tnew\n", 10 * day, 100 * day)).unwrap();

        let entries = recent_from(&path, 30, 100 * day).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "new");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_empty_rather_than_an_error() {
        let path = temp("missing");
        assert!(recent_from(&path, 30, 1000).unwrap().is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped_rather_than_failing_the_read() {
        let path = temp("malformed");
        std::fs::write(&path, "not-a-record\n500\tkept\n").unwrap();
        let entries = recent_from(&path, 30, 600).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "kept");
        let _ = std::fs::remove_file(&path);
    }
}
