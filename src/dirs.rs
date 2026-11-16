//! Where the app keeps its files.
//!
//! Linux follows the XDG base directories. Windows has none, and usually has no
//! `HOME`, so it uses `APPDATA` for configuration and `LOCALAPPDATA` for
//! everything machine-local: log, endpoint, model and cache.
//!
//! Resolution takes the environment as a function rather than reading it
//! directly, so both rule sets can be tested on either platform.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::compat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Config,
    State,
    Data,
    Runtime,
    Cache,
}

impl Kind {
    fn xdg(self) -> &'static str {
        match self {
            Kind::Config => "XDG_CONFIG_HOME",
            Kind::State => "XDG_STATE_HOME",
            Kind::Data => "XDG_DATA_HOME",
            Kind::Runtime => "XDG_RUNTIME_DIR",
            Kind::Cache => "XDG_CACHE_HOME",
        }
    }

    /// The fallback below `$HOME` when the XDG variable is unset.
    fn home_relative(self) -> &'static [&'static str] {
        match self {
            Kind::Config => &[".config"],
            Kind::State => &[".local", "state"],
            Kind::Data => &[".local", "share"],
            Kind::Runtime => &[".local", "state"],
            Kind::Cache => &[".cache"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Xdg,
    Windows,
}

fn family() -> Family {
    if cfg!(windows) {
        Family::Windows
    } else {
        Family::Xdg
    }
}

fn resolve(kind: Kind) -> Result<PathBuf> {
    let env = |key: &str| std::env::var_os(key);
    // The runtime directory is for sockets and short-lived files. Where there
    // is none, the state directory is the nearest thing — resolved through the
    // same adoption path, so it cannot point at a tree the app has moved away
    // from.
    if kind == Kind::Runtime && family() == Family::Xdg && env("XDG_RUNTIME_DIR").is_none() {
        return resolve(Kind::State);
    }
    let root = root_from(family(), kind, &env)?;
    Ok(with_cache_suffix(family(), kind, root))
}

/// Windows keeps its cache in a `cache` subdirectory of the app root; every
/// other base directory is the root itself.
fn with_cache_suffix(family: Family, kind: Kind, root: PathBuf) -> PathBuf {
    if family == Family::Windows && kind == Kind::Cache {
        root.join("cache")
    } else {
        root
    }
}

/// The app root for `kind`, adopting a directory from a previous name when this
/// generation has not written one yet.
fn root_from(
    family: Family,
    kind: Kind,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf> {
    let current = resolve_for(family, kind, compat::APP, env)?;
    if current.exists() {
        return Ok(current);
    }
    for legacy_app in compat::LEGACY_APPS {
        let legacy = resolve_for(family, kind, legacy_app, env)?;
        if legacy.exists() {
            return Ok(compat::adopt_dir(current, legacy));
        }
    }
    Ok(current)
}

fn resolve_for(
    family: Family,
    kind: Kind,
    app: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf> {
    match family {
        Family::Xdg => {
            // The runtime directory is for sockets and short-lived files. Where
            // there is none, the state directory is the nearest thing.
            if kind == Kind::Runtime {
                if let Some(dir) = env("XDG_RUNTIME_DIR") {
                    return Ok(PathBuf::from(dir).join(app));
                }
                return resolve_for(Family::Xdg, Kind::State, app, env);
            }
            if let Some(dir) = env(kind.xdg()) {
                return Ok(PathBuf::from(dir).join(app));
            }
            let home = env("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("neither {} nor HOME is set", kind.xdg()))?;
            let base = kind
                .home_relative()
                .iter()
                .fold(home, |path, part| path.join(part));
            Ok(base.join(app))
        }
        Family::Windows => {
            let root = match kind {
                // Configuration roams; everything else is per-machine.
                Kind::Config => env("APPDATA")
                    .or_else(|| env("LOCALAPPDATA"))
                    .or_else(|| env("USERPROFILE")),
                _ => env("LOCALAPPDATA")
                    .or_else(|| env("APPDATA"))
                    .or_else(|| env("USERPROFILE")),
            }
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("neither LOCALAPPDATA, APPDATA nor USERPROFILE is set"))?;

            Ok(root.join(app))
        }
    }
}

pub fn config_dir() -> Result<PathBuf> {
    resolve(Kind::Config)
}

pub fn state_dir() -> Result<PathBuf> {
    resolve(Kind::State)
}

pub fn data_dir() -> Result<PathBuf> {
    resolve(Kind::Data)
}

pub fn runtime_dir() -> Result<PathBuf> {
    resolve(Kind::Runtime)
}

pub fn cache_dir() -> Result<PathBuf> {
    resolve(Kind::Cache)
}

/// The variable that names the config file outright, for tests.
///
/// Integration tests drive the real binary, so they cannot rely on the XDG
/// variables alone: a developer's environment would leak into the run. Naming
/// the file directly is also the only way to move just the config and leave the
/// log, socket and history where the rest of the suite expects them.
pub fn config_file() -> Result<PathBuf> {
    config_file_from(&|key| std::env::var_os(key))
}

fn config_file_from(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    // Both generations of the name, so a test harness written against the old
    // one keeps working.
    if let Some(path) = env(&format!("{}_CONFIG", compat::PREFIX))
        .or_else(|| env(&format!("{}_CONFIG", compat::LEGACY_PREFIX)))
    {
        return Ok(PathBuf::from(path));
    }
    Ok(root_from(family(), Kind::Config, env)?.join("config.toml"))
}

pub fn log_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("log"))
}

pub fn history_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("history.tsv"))
}

/// The file a Windows client reads to find the daemon's port and token.
pub fn endpoint_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("endpoint"))
}

/// A pane request from the tray icon to the already-running settings window.
///
/// The settings binary is a separate process, so a second click cannot pass an
/// argument to the first. The tray writes this file before focusing the window,
/// and the window reads and deletes it.
pub fn settings_request_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("settings-request"))
}

/// The Unix socket the daemon listens on.
pub fn socket_file() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.sock"))
}

/// A generated cue WAV.
pub fn cue_file(name: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("cue-{name}.wav")))
}

pub fn model_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("models"))
}

/// The signed licence, if one is installed. Data, not configuration: it is
/// something the user was given, not something they edit.
pub fn licence_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("licence"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| map.get(key).map(OsString::from)
    }

    #[test]
    fn xdg_variables_win_over_home() {
        let env = env_of(&[
            ("XDG_CONFIG_HOME", "/xdg/config"),
            ("XDG_STATE_HOME", "/xdg/state"),
            ("XDG_DATA_HOME", "/xdg/data"),
            ("XDG_CACHE_HOME", "/xdg/cache"),
            ("XDG_RUNTIME_DIR", "/xdg/run"),
            ("HOME", "/home/me"),
        ]);
        assert_eq!(
            resolve_for(Family::Xdg, Kind::Config, compat::APP, &env).unwrap(),
            PathBuf::from("/xdg/config/myware-voice")
        );
        assert_eq!(
            resolve_for(Family::Xdg, Kind::State, compat::APP, &env).unwrap(),
            PathBuf::from("/xdg/state/myware-voice")
        );
        assert_eq!(
            resolve_for(Family::Xdg, Kind::Runtime, compat::APP, &env).unwrap(),
            PathBuf::from("/xdg/run/myware-voice")
        );
        assert_eq!(
            resolve_for(Family::Xdg, Kind::Cache, compat::APP, &env).unwrap(),
            PathBuf::from("/xdg/cache/myware-voice")
        );
    }

    #[test]
    fn home_is_the_unix_fallback() {
        let env = env_of(&[("HOME", "/home/me")]);
        assert_eq!(
            resolve_for(Family::Xdg, Kind::Config, compat::APP, &env).unwrap(),
            PathBuf::from("/home/me/.config/myware-voice")
        );
        assert_eq!(
            resolve_for(Family::Xdg, Kind::Data, compat::APP, &env).unwrap(),
            PathBuf::from("/home/me/.local/share/myware-voice")
        );
        // No runtime dir: the state directory stands in.
        assert_eq!(
            resolve_for(Family::Xdg, Kind::Runtime, compat::APP, &env).unwrap(),
            PathBuf::from("/home/me/.local/state/myware-voice")
        );
    }

    #[test]
    fn config_override_names_the_file_itself() {
        let env = env_of(&[
            ("MYWARE_VOICE_CONFIG", "/tmp/elsewhere/config.toml"),
            ("XDG_CONFIG_HOME", "/xdg/config"),
        ]);
        assert_eq!(
            config_file_from(&env).unwrap(),
            PathBuf::from("/tmp/elsewhere/config.toml")
        );

        let plain = env_of(&[("XDG_CONFIG_HOME", "/xdg/config")]);
        assert_eq!(
            config_file_from(&plain).unwrap(),
            PathBuf::from("/xdg/config/myware-voice/config.toml")
        );
    }

    /// The rules that only run on Windows, exercised on any host.
    #[test]
    fn windows_separates_config_from_machine_state() {
        let env = env_of(&[
            ("APPDATA", r"C:\Users\me\AppData\Roaming"),
            ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
        ]);
        assert_eq!(
            resolve_for(Family::Windows, Kind::Config, compat::APP, &env).unwrap(),
            PathBuf::from(r"C:\Users\me\AppData\Roaming").join("myware-voice")
        );
        assert_eq!(
            resolve_for(Family::Windows, Kind::State, compat::APP, &env).unwrap(),
            PathBuf::from(r"C:\Users\me\AppData\Local").join("myware-voice")
        );
        assert_eq!(
            with_cache_suffix(
                Family::Windows,
                Kind::Cache,
                resolve_for(Family::Windows, Kind::Cache, compat::APP, &env).unwrap(),
            ),
            PathBuf::from(r"C:\Users\me\AppData\Local")
                .join("myware-voice")
                .join("cache")
        );
        // HOME is irrelevant there, and its absence must not be an error.
        assert_eq!(
            resolve_for(Family::Windows, Kind::Runtime, compat::APP, &env).unwrap(),
            PathBuf::from(r"C:\Users\me\AppData\Local").join("myware-voice")
        );
    }

    #[test]
    fn windows_falls_back_to_the_profile_and_fails_loudly_without_one() {
        let profile = env_of(&[("USERPROFILE", r"C:\Users\me")]);
        assert_eq!(
            resolve_for(Family::Windows, Kind::Config, compat::APP, &profile).unwrap(),
            PathBuf::from(r"C:\Users\me").join("myware-voice")
        );

        let empty = env_of(&[]);
        let error = resolve_for(Family::Windows, Kind::State, compat::APP, &empty).unwrap_err();
        assert!(error.to_string().contains("LOCALAPPDATA"));
    }
}
