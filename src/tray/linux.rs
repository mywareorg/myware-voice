//! The tray icon on Linux: a StatusNotifierItem, hosted by whatever owns
//! `org.kde.StatusNotifierWatcher` — waybar's tray module, most often.
//!
//! Nothing here is load-bearing. A session with no tray host, a bus that is not
//! there, or a watcher that starts after the daemon all mean "no icon", not "no
//! dictation": the spawn is best-effort and retried in the background, because a
//! daemon that refused to start without a tray would be a worse bug than a
//! missing menu.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::Result;
use ksni::blocking::TrayMethods as _;
use ksni::menu::StandardItem;
use ksni::MenuItem;

use crate::daemon::Event;

/// The item the shell talks to. It holds only what the menu callbacks need.
#[derive(Debug)]
struct MyWare {
    events: Sender<Event>,
}

impl MyWare {
    /// Open the settings window, or the history pane of it.
    ///
    /// A request file is what a running window reads to switch panes; a window
    /// that is not running is started with the pane in its environment. If one is
    /// already open, the settings binary's own handshake hands the pane over and
    /// raises that window instead of stacking another.
    fn open(&self, pane: &str) {
        let Some(binary) = settings_binary() else {
            crate::log::error("tray: myware-voice-settings not found; cannot open the window");
            return;
        };
        if let Ok(path) = crate::dirs::settings_request_file() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, pane);
        }
        let spawned = std::process::Command::new(binary)
            .env(crate::compat::env_name("START_PANE"), pane)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(error) = spawned {
            crate::log::error(&format!(
                "tray: could not start the settings window: {error}"
            ));
        }
    }
}

impl ksni::Tray for MyWare {
    fn id(&self) -> String {
        "myware-voice".to_string()
    }

    fn title(&self) -> String {
        "MyWare Voice".to_string()
    }

    /// The name the icon theme is asked for, which is what `install.sh` puts under
    /// `hicolor`. A development checkout that has installed nothing shows the
    /// host's fallback icon rather than no icon.
    fn icon_name(&self) -> String {
        "myware-voice".to_string()
    }

    /// Left click, which is what everyone tries first.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.open("settings");
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open settings\u{2026}".to_string(),
                activate: Box::new(|this: &mut Self| this.open("settings")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Recent transcripts".to_string(),
                activate: Box::new(|this: &mut Self| this.open("history")),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Toggle dictation".to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.events.send(Event::Toggle);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.events.send(Event::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// The settings binary: beside this one in an installed tree, on `PATH` for a
/// development checkout.
fn settings_binary() -> Option<PathBuf> {
    let sibling = std::env::current_exe()
        .ok()?
        .parent()?
        .join("myware-voice-settings");
    if sibling.is_file() {
        return Some(sibling);
    }
    crate::inject::find("myware-voice-settings")
}

/// Start the tray in the background. Never fails the daemon.
pub fn spawn(events: Sender<Event>) -> Result<()> {
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            // The watcher belongs to the bar, which may start long after us —
            // waybar relaunching after a crash, or a slow session — so the window
            // is half a minute. One cheap D-Bus attempt a second, and nothing at
            // all once it succeeds.
            for attempt in 1..=30 {
                match (MyWare {
                    events: events.clone(),
                })
                .spawn()
                {
                    Ok(handle) => {
                        // The service is meant to outlive this call. Nothing in
                        // ksni shuts it down on drop today, and forgetting the
                        // handle keeps that true if it ever changes.
                        std::mem::forget(handle);
                        crate::log::write("tray: registered with the StatusNotifier watcher");
                        return;
                    }
                    Err(error) if attempt == 30 => {
                        crate::log::error(&format!(
                            "tray: no StatusNotifier host after 30 attempts: {error}"
                        ));
                        return;
                    }
                    Err(_) => std::thread::sleep(Duration::from_secs(1)),
                }
            }
        })?;
    Ok(())
}
