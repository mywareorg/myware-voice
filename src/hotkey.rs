//! Windows global hotkey.
//!
//! A command line cannot be run by a compositor on Windows the way sway runs
//! `hippo-voice toggle`, so the daemon registers the chord itself.
//! `RegisterHotKey` with a null window posts `WM_HOTKEY` to the calling thread's
//! queue, so this needs a thread with a message loop rather than a window
//! procedure.
//!
//! `RegisterHotKey` is deliberate. A low-level hook would also work, but it puts
//! every keystroke on the system through this process, trips antivirus
//! heuristics, and is what anti-cheat software blocks.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

use crate::chord::{Chord, Key};
use crate::daemon::{Event, Status};

const HOTKEY_ID: i32 = 1;

/// Register the chord on its own thread and forward each press.
///
/// The outcome is written into the shared status either way: a daemon that looks
/// alive but never fires is the worst failure this app can have, so a taken chord
/// has to be visible in `hippo-voice status`.
pub fn spawn(events: Sender<Event>, chord: Chord, status: Arc<Mutex<Status>>) -> Result<()> {
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || unsafe {
            let modifiers = modifiers(&chord) | MOD_NOREPEAT;
            let key = virtual_key(chord.key);
            let registered = RegisterHotKey(std::ptr::null_mut(), HOTKEY_ID, modifiers, key) != 0;

            let outcome = if registered {
                chord.to_string()
            } else {
                format!("unavailable: {chord} is taken by another program")
            };
            crate::log::write(&format!("hotkey: {outcome}"));
            if let Ok(mut view) = status.lock() {
                view.hotkey = Some(outcome);
            }
            if !registered {
                return;
            }

            let mut message: MSG = std::mem::zeroed();
            loop {
                let result = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
                if result <= 0 {
                    break;
                }
                if message.message == WM_HOTKEY && message.wParam == HOTKEY_ID as usize {
                    if events.send(Event::Toggle).is_err() {
                        break;
                    }
                }
            }
        })?;
    Ok(())
}

fn modifiers(chord: &Chord) -> u32 {
    let mut flags = 0;
    if chord.alt {
        flags |= MOD_ALT;
    }
    if chord.ctrl {
        flags |= MOD_CONTROL;
    }
    if chord.shift {
        flags |= MOD_SHIFT;
    }
    if chord.win {
        flags |= MOD_WIN;
    }
    flags
}

fn virtual_key(key: Key) -> u32 {
    match key {
        Key::Space => 0x20,
        Key::Enter => 0x0D,
        Key::Tab => 0x09,
        Key::Escape => 0x1B,
        Key::Backspace => 0x08,
        Key::Function(number) => 0x70 + (number as u32 - 1),
        // Letters, digits and the ASCII punctuation keys use their ASCII value
        // as the virtual-key code.
        Key::Char(character) => character.to_ascii_uppercase() as u32,
    }
}
