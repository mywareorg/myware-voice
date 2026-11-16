//! Names this app answers to, and the lookups that outlive a rename.
//!
//! The app has been renamed before and will be renamed again. Anything a user
//! can set, or that an installation has already written to disk, has to answer
//! to both generations: the current name first, the previous one second.
//!
//! `PREFIX` and `APP` are the whole rename. `LEGACY_PREFIX` and `LEGACY_APPS`
//! keep the old names readable, so an upgrade never starts from an empty
//! configuration or re-downloads a 640 MB model.

use std::ffi::OsString;
use std::path::PathBuf;

/// The variable prefix, without the trailing underscore.
pub const PREFIX: &str = "MYWARE_VOICE";

/// The prefix the previous generation used.
///
/// This line alone keeps installations that set the old variables working.
pub const LEGACY_PREFIX: &str = "HIPPO_DICTATE";

/// The app's directory name under each base directory.
pub const APP: &str = "myware-voice";

/// Directory names this app has lived under, newest first.
///
/// The first one that exists on disk is renamed into place the first time a
/// directory is resolved. Renaming is instant on one volume; if it fails, the
/// legacy tree keeps being used rather than losing what is in it.
pub const LEGACY_APPS: &[&str] = &["hippo-dictate"];

/// The full name of a variable, for callers that hand it to `Command::env`.
pub fn env_name(suffix: &str) -> String {
    format!("{PREFIX}_{suffix}")
}

fn read(prefix: &str, suffix: &str) -> Option<OsString> {
    std::env::var_os(format!("{prefix}_{suffix}"))
}

/// A variable under the current name, falling back to the previous one.
pub fn var_os(suffix: &str) -> Option<OsString> {
    read(PREFIX, suffix).or_else(|| read(LEGACY_PREFIX, suffix))
}

/// A variable under the current name, decoded as UTF-8, falling back to the
/// previous one.
pub fn var(suffix: &str) -> Option<String> {
    var_os(suffix).and_then(|value| value.into_string().ok())
}

/// Adopt a directory left behind under a previous name.
///
/// Returns `new` when it already exists, or when there is no `legacy` tree to
/// take over. Otherwise tries a single rename (instant on the same volume); if
/// that fails, keeps using `legacy` so downloaded models and history survive.
pub fn adopt_dir(new: PathBuf, legacy: PathBuf) -> PathBuf {
    if new.exists() || !legacy.exists() {
        return new;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::rename(&legacy, &new).is_ok() {
        new
    } else {
        legacy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "myware-voice-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn a_directory_from_the_previous_name_is_adopted() {
        let base = scratch("adopt");
        let legacy = base.join("legacy");
        let current = base.join("current");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("history.tsv"), "one\n").unwrap();

        let adopted = adopt_dir(current.clone(), legacy.clone());

        assert_eq!(adopted, current);
        assert_eq!(
            std::fs::read_to_string(current.join("history.tsv")).unwrap(),
            "one\n"
        );
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_existing_directory_wins_over_the_previous_name() {
        let base = scratch("adopt-existing");
        let legacy = base.join("legacy");
        let current = base.join("current");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(legacy.join("old"), "").unwrap();
        std::fs::write(current.join("new"), "").unwrap();

        assert_eq!(adopt_dir(current.clone(), legacy.clone()), current);
        assert!(legacy.join("old").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_missing_previous_directory_leaves_the_current_path_alone() {
        let base = scratch("adopt-missing");
        let legacy = base.join("legacy");
        let current = base.join("current");

        assert_eq!(adopt_dir(current.clone(), legacy), current);
        assert!(!current.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
