//! Short cues, because there is no window to look at.
//!
//! A dictation app that records silently gives the user no way to tell
//! "recording" from "the daemon is hung". Two cues are enough: a rising note when
//! capture starts and a falling one when a transcript has been delivered.
//!
//! Playing is fire-and-forget on its own thread: opening an output device can
//! take tens of milliseconds, and that must never sit between the keypress and
//! the microphone opening.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    Start,
    Done,
}

impl Cue {
    fn name(self) -> &'static str {
        match self {
            Cue::Start => "start",
            Cue::Done => "done",
        }
    }

    fn frequency(self) -> f32 {
        match self {
            Cue::Start => 880.0,
            Cue::Done => 587.33,
        }
    }

    fn milliseconds(self) -> u32 {
        match self {
            Cue::Start => 70,
            Cue::Done => 90,
        }
    }
}

/// Play a cue, or do nothing if there is no player or no runtime directory.
pub fn play(cue: Cue) {
    std::thread::spawn(move || {
        if let Err(error) = play_blocking(cue) {
            eprintln!("cue: {error}");
        }
    });
}

fn play_blocking(cue: Cue) -> Result<(), String> {
    let path = cue_path(cue)?;
    if !path.is_file() {
        let samples = render(cue.frequency(), cue.milliseconds(), 48_000);
        crate::audio::write_wav(&path, &samples, 48_000)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    }

    #[cfg(windows)]
    {
        return play_windows(&path);
    }

    #[cfg(not(windows))]
    {
        play_with_player(&path)
    }
}

/// `PlaySoundW` is the whole Windows cue path: no child process to reap, and
/// `SND_ASYNC` keeps it off the capture thread.
#[cfg(windows)]
fn play_windows(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let played = unsafe {
        PlaySoundW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            SND_ASYNC | SND_FILENAME | SND_NODEFAULT,
        )
    };
    if played == 0 {
        return Err(format!("PlaySoundW could not play {}", path.display()));
    }
    Ok(())
}

#[cfg(not(windows))]
fn play_with_player(path: &Path) -> Result<(), String> {
    let Some(player) = super::inject::have("pw-play")
        .then_some("pw-play")
        .or_else(|| super::inject::have("paplay").then_some("paplay"))
    else {
        return Ok(());
    };

    let mut child = std::process::Command::new(player)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("spawn {player}: {error}"))?;
    // Reap it here: this thread exists for the length of the cue, and a daemon
    // that spawns a player per cue must not accumulate zombies.
    let _ = child.wait();
    Ok(())
}

fn cue_path(cue: Cue) -> Result<PathBuf, String> {
    let path = crate::dirs::cue_file(cue.name()).map_err(|error| error.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("create {}: {error}", dir.display()))?;
    }
    Ok(path)
}

/// A sine burst with a short fade at each end, so the speaker does not click.
fn render(frequency: f32, milliseconds: u32, rate: u32) -> Vec<f32> {
    let total = (rate * milliseconds / 1000) as usize;
    let fade = (rate / 200).max(1) as usize; // 5 ms
    let amplitude = 0.12f32;
    let step = std::f32::consts::TAU * frequency / rate as f32;

    (0..total)
        .map(|index| {
            let envelope = if index < fade {
                index as f32 / fade as f32
            } else if index + fade > total {
                (total - index) as f32 / fade as f32
            } else {
                1.0
            };
            (step * index as f32).sin() * amplitude * envelope.clamp(0.0, 1.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cue_is_the_requested_length_and_starts_and_ends_silent() {
        let samples = render(880.0, 70, 48_000);
        assert_eq!(samples.len(), 48_000 * 70 / 1000);
        assert_eq!(samples[0], 0.0);
        assert!(samples[samples.len() - 1].abs() < 0.001);
        assert!(samples.iter().any(|sample| sample.abs() > 0.05));
    }

    #[test]
    fn the_two_cues_are_different_notes() {
        assert_ne!(Cue::Start.frequency(), Cue::Done.frequency());
        assert!(Cue::Start.frequency() > Cue::Done.frequency());
    }
}
