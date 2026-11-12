//! Windows global hotkey.
//!
//! A command line cannot be run by a compositor on Windows the way sway runs
//! `hippo-voice toggle`, so the daemon registers the chord itself. `RegisterHotKey`
//! with a null window posts `WM_HOTKEY` to the calling thread's queue, so this
//! needs a thread with a message loop rather than a window procedure.

use std::sync::mpsc::Sender;

use anyhow::Result;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_CONTROL, MOD_NOREPEAT};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

use crate::daemon::Event;

const VK_SPACE: u32 = 0x20;
const HOTKEY_ID: i32 = 1;

/// Register `ctrl+space` on its own thread and forward each press.
///
/// The thread never exits: it blocks in `GetMessageW` for the life of the daemon.
pub fn spawn(events: Sender<Event>) -> Result<()> {
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || unsafe {
            if RegisterHotKey(
                std::ptr::null_mut(),
                HOTKEY_ID,
                (MOD_CONTROL | MOD_NOREPEAT) as u32,
                VK_SPACE,
            ) == 0
            {
                eprintln!("hotkey: ctrl+space is already taken by another program");
                crate::log::write("hotkey: ctrl+space is already taken by another program");
                return;
            }
            crate::log::write("hotkey: ctrl+space registered");

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
