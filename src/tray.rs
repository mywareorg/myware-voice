//! The notification-area icon.
//!
//! A hidden window, `Shell_NotifyIconW`, and a popup menu. The app has no other
//! visible surface on Windows, so this is how a user reaches settings, the
//! transcript history and Quit without a console.
//!
//! The menu launches the settings binary rather than talking to the daemon: that
//! window edits `config.toml` directly and the daemon reads the file on the next
//! capture, so there is no IPC to keep in step.

use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetMessageW, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, IDI_APPLICATION, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, MSG, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

use crate::daemon::Event;

/// Callback message the shell sends for clicks on the icon.
const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;

const CMD_SETTINGS: usize = 1;
const CMD_HISTORY: usize = 2;
const CMD_QUIT: usize = 3;

struct Context {
    events: Sender<Event>,
}

static CONTEXT: OnceLock<Mutex<Option<Context>>> = OnceLock::new();

/// The window the icon belongs to, so `notify` can reach it from any thread.
///
/// Stored as an integer because `HWND` is a raw pointer and a static must be
/// `Send`.
static ICON_WINDOW: OnceLock<Mutex<Option<usize>>> = OnceLock::new();

fn icon_window() -> &'static Mutex<Option<usize>> {
    ICON_WINDOW.get_or_init(|| Mutex::new(None))
}

/// Show a balloon from the tray icon.
///
/// This is the Windows half of the delivery fallback: when the paste cannot be
/// made, the transcript is on the clipboard and this is how the user finds out.
pub fn notify(title: &str, body: &str) {
    let Some(window) = icon_window().lock().ok().and_then(|slot| *slot) else {
        return;
    };
    let window = window as HWND;

    unsafe {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = window;
        data.uID = ICON_ID;
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_WARNING;
        copy_utf16(&mut data.szInfoTitle, title);
        copy_utf16(&mut data.szInfo, body);
        Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

/// Copy into a fixed NUL-terminated UTF-16 field, truncating on a char boundary.
fn copy_utf16<const N: usize>(field: &mut [u16; N], text: &str) {
    if N == 0 {
        return;
    }
    let mut written = 0;
    for unit in text.encode_utf16().take(N - 1) {
        field[written] = unit;
        written += 1;
    }
    field[written] = 0;
}

fn context() -> &'static Mutex<Option<Context>> {
    CONTEXT.get_or_init(|| Mutex::new(None))
}

/// Add the icon and pump its messages on a thread of its own.
pub fn spawn(events: Sender<Event>) -> Result<()> {
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            if let Err(error) = run(events) {
                crate::log::write(&format!("tray: {error:#}"));
            }
        })?;
    Ok(())
}

fn run(events: Sender<Event>) -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name: Vec<u16> = "HippoVoiceTray\0".encode_utf16().collect();

        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&class) == 0 {
            anyhow::bail!("could not register the tray window class");
        }

        // A normal window that is never shown: it exists to receive the icon's
        // callbacks, and it must be a real window for the shell to accept it.
        let window = CreateWindowExW(
            0,
            class_name.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        if window.is_null() {
            anyhow::bail!("could not create the tray window");
        }

        *context().lock().unwrap() = Some(Context {
            events: events.clone(),
        });

        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = window;
        data.uID = ICON_ID;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        // The stock application icon, until the mark is built in as a resource.
        data.hIcon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
        let tip: Vec<u16> = "Hippo Voice - press the hotkey to dictate\0"
            .encode_utf16()
            .collect();
        for (slot, value) in data.szTip.iter_mut().zip(tip.iter()) {
            *slot = *value;
        }

        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            anyhow::bail!("the shell refused the notification-area icon");
        }
        *icon_window().lock().unwrap() = Some(window as usize);
        crate::log::write("tray: icon added");

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }

        Shell_NotifyIconW(NIM_DELETE, &data);
        *icon_window().lock().unwrap() = None;
        Ok(())
    }
}

unsafe fn build_menu() -> *mut std::ffi::c_void {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return menu;
    }

    let header = wide("Hippo Voice");
    AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, header.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    let settings = wide("Settings and history\u{2026}");
    AppendMenuW(menu, MF_STRING, CMD_SETTINGS, settings.as_ptr());
    let history = wide("Recent transcripts");
    AppendMenuW(menu, MF_STRING, CMD_HISTORY, history.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    let quit = wide("Quit");
    AppendMenuW(menu, MF_STRING, CMD_QUIT, quit.as_ptr());
    menu
}

fn wide(text: &str) -> Vec<u16> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    utf16
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            let event = lparam as u32;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                show_menu(window);
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

unsafe fn show_menu(window: HWND) {
    let events = context()
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|context| context.events.clone()));
    let Some(events) = events else { return };

    let menu = build_menu();
    if menu.is_null() {
        return;
    }

    // Windows wants the owner in the foreground before tracking, and a posted
    // null message afterwards, or the menu does not dismiss on an outside click.
    SetForegroundWindow(window);
    let choice = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        0,
        0,
        0,
        window,
        std::ptr::null(),
    ) as usize;
    PostMessageW(window, WM_NULL, 0, 0);
    DestroyMenu(menu);

    match choice {
        CMD_SETTINGS => open_settings(false),
        CMD_HISTORY => open_settings(true),
        CMD_QUIT => {
            let _ = events.send(Event::Quit);
            DestroyWindow(window);
        }
        _ => {}
    }
}

/// Launch the settings binary that sits beside this executable.
fn open_settings(history: bool) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else { return };
    let path = dir.join("hippo-voice-settings.exe");
    if !path.is_file() {
        crate::log::write(&format!("tray: {} is not installed", path.display()));
        return;
    }

    let mut command = std::process::Command::new(path);
    if history {
        command.env("HIPPO_VOICE_START_PANE", "history");
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
    if let Err(error) = command.spawn() {
        crate::log::write(&format!("tray: could not start settings: {error}"));
    }
}
