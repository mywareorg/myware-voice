//! The listening popup.
//!
//! A small toast in the bottom-right corner, where Windows notifications live:
//! a dark card, the word "Listening", a timer and a frequency meter. It is
//! created with `WS_EX_NOACTIVATE` and shown with `SW_SHOWNOACTIVATE`; the
//! daemon must never take focus, or the transcript would paste into the wrong
//! window.
//!
//! The meter is fed by `Status::bands`, which the daemon computes from the live
//! capture every 100 ms as one magnitude per frequency band. Columns stay in
//! place and move only vertically. The window itself is plain GDI: one fill, one
//! font, a row of rectangles. There is no renderer to keep warm for a surface
//! that is visible for a few seconds at a time.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, EndPaint,
    FillRect, GetMonitorInfoW, InvalidateRect, MonitorFromWindow, SelectClipRgn, SelectObject,
    SetBkMode, SetTextAlign, SetTextColor, SetWindowRgn, TextOutW, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, TA_RIGHT, TA_TOP,
    TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetForegroundWindow,
    PeekMessageW, PostQuitMessage, RegisterClassW, SetWindowPos, ShowWindow, SystemParametersInfoW,
    TranslateMessage, HWND_TOPMOST, MSG, PM_REMOVE, SPI_GETWORKAREA, SWP_NOACTIVATE,
    SWP_NOOWNERZORDER, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WM_DESTROY, WM_ERASEBKGND, WM_PAINT,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::daemon::{Phase, Status};

const WIDTH: i32 = 227;
const HEIGHT: i32 = 116;
const MARGIN: i32 = 16;
const TICK: Duration = Duration::from_millis(16);
/// Per-frame easing toward the newest band value. Attack is quicker than decay
/// so consonants register, but neither snaps: the daemon publishes bands at
/// 10 Hz, and 60 Hz interpolation is what makes the meter look fluid.
const ATTACK: f32 = 0.45;
const DECAY: f32 = 0.10;

/// The level history and timer the window procedure draws. The run loop owns
/// the writes; `WM_PAINT` only reads a copy under the lock.
struct Meter {
    /// Smoothed fill, 0..1, for the microphone glyph.
    fill: f32,
    seconds: f32,
    streaming: bool,
}

static METER: OnceLock<Mutex<Meter>> = OnceLock::new();

fn meter() -> &'static Mutex<Meter> {
    METER.get_or_init(|| {
        Mutex::new(Meter {
            fill: 0.0,
            seconds: 0.0,
            streaming: false,
        })
    })
}

/// Add the popup and poll the daemon's status from its own thread.
pub fn spawn(status: Arc<Mutex<Status>>) -> Result<()> {
    std::thread::Builder::new()
        .name("overlay".into())
        .spawn(move || {
            if let Err(error) = run(status) {
                crate::log::error(&format!("overlay: {error:#}"));
            }
        })?;
    Ok(())
}

fn run(status: Arc<Mutex<Status>>) -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("HippoVoiceOverlay");

        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&class) == 0 {
            anyhow::bail!("could not register the overlay window class");
        }

        let title = wide("");
        let window = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            WIDTH,
            HEIGHT,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() {
            anyhow::bail!("could not create the overlay window");
        }

        // Rounded corners are a window region, not a theme: Windows 10 has no
        // DWM corner preference, and the card should look the same on both.
        let region = CreateRoundRectRgn(0, 0, WIDTH + 1, HEIGHT + 1, 18, 18);
        if !region.is_null() {
            SetWindowRgn(window, region, 1);
        }

        let mut fill = 0.0f32;
        let mut shown = false;
        let mut message: MSG = std::mem::zeroed();

        loop {
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }

            let (recording, level, seconds, streaming) = match status.lock() {
                Ok(status) => (
                    status.phase == Phase::Recording,
                    status.level,
                    status.seconds,
                    status.streaming,
                ),
                Err(_) => (false, 0.0, 0.0, false),
            };

            if recording {
                if !shown {
                    fill = 0.0;
                    place(window);
                    ShowWindow(window, SW_SHOWNOACTIVATE);
                    shown = true;
                }
                // Rise at once, fall gently: the same curve as the Linux overlay,
                // so a syllable lights the glyph and a pause drains it.
                let target = (level * 3.2).clamp(0.0, 1.0).powf(0.7);
                let easing = if target > fill { ATTACK } else { DECAY };
                fill += (target - fill) * easing;
                if let Ok(mut meter) = meter().lock() {
                    meter.fill = fill;
                    meter.seconds = seconds;
                    meter.streaming = streaming;
                }
                // Paint through WM_PAINT so the class remains a normal window;
                // drawing from two threads into one HDC is the kind of thing
                // that works until a repaint lands mid-frame.
                InvalidateRect(window, std::ptr::null(), 0);
            } else if shown {
                ShowWindow(window, SW_HIDE);
                shown = false;
            }

            std::thread::sleep(TICK);
        }
    }
}

/// Put the card in the top right of the work area, the same corner as the Linux
/// popup. Recomputed on every show: the taskbar can move, and the user can move
/// between monitors.
unsafe fn place(window: HWND) {
    // The work area of the monitor the user is typing on, not the primary one:
    // the popup is meant to be seen where the transcript is going. The fallback
    // is the system work area, for a session with no foreground window.
    let mut area: RECT = std::mem::zeroed();
    let foreground = GetForegroundWindow();
    let monitor = if foreground.is_null() {
        std::ptr::null_mut()
    } else {
        MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST)
    };
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
        area = info.rcWork;
    } else {
        let found = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut area as *mut RECT as *mut std::ffi::c_void,
            0,
        );
        if found == 0 {
            return;
        }
    }
    // Top right, the same corner as the Linux popup.
    let x = (area.right - WIDTH - MARGIN).max(area.left);
    let y = (area.top + MARGIN).max(area.top);
    SetWindowPos(
        window,
        HWND_TOPMOST,
        x,
        y,
        0,
        0,
        SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
    );
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_PAINT => {
            paint(window);
            0
        }
        // The card is fully painted every frame; letting Windows erase first
        // flashes the desktop through the rounded corners.
        WM_ERASEBKGND => 1,
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

unsafe fn paint(window: HWND) {
    let mut paint: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(window, &mut paint);

    let mut rect: RECT = std::mem::zeroed();
    GetClientRect(window, &mut rect);
    let width = rect.right;

    let background = CreateSolidBrush(rgb(28, 28, 32));
    FillRect(dc, &rect, background);
    DeleteObject(background);

    let (fill, seconds, streaming) = match meter().lock() {
        Ok(meter) => (meter.fill, meter.seconds, meter.streaming),
        Err(_) => (0.0, 0.0, false),
    };

    let font = CreateFontW(
        -15,
        0,
        0,
        0,
        600,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        0,
        0,
        CLEARTYPE_QUALITY as u32,
        0,
        wide("Segoe UI").as_ptr(),
    );
    let previous = SelectObject(dc, font as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, rgb(240, 240, 244));

    let label = wide(if streaming { "Streaming" } else { "Listening" });
    TextOutW(dc, 32, 14, label.as_ptr(), label.len() as i32 - 1);

    SetTextAlign(dc, TA_RIGHT | TA_TOP);
    SetTextColor(dc, rgb(154, 160, 171));
    let timer = wide(&format!("{seconds:.1}s"));
    TextOutW(dc, width - 18, 16, timer.as_ptr(), timer.len() as i32 - 1);
    SetTextAlign(dc, TA_TOP);

    SelectObject(dc, previous);
    DeleteObject(font);

    // The microphone: a capsule that fills from the bottom with the level, the
    // same shape and the same colour rule as the Linux overlay. The clip region is
    // what lets a plain FillRect fill only the lower part of a rounded shape.
    let cx = width / 2;
    let cap_top = 46;
    let cap_bottom = rect.bottom - 32;
    let cap_width = 24;
    let left = cx - cap_width / 2;
    let right = cx + cap_width / 2;
    let (r, g, b) = if streaming {
        (91, 155, 213)
    } else {
        (76, 175, 122)
    };

    let capsule = CreateRoundRectRgn(left, cap_top, right, cap_bottom, cap_width, cap_width);
    let track = CreateSolidBrush(rgb(58, 60, 68));
    let lit = CreateSolidBrush(rgb(r, g, b));
    SelectClipRgn(dc, capsule);
    let whole = RECT {
        left,
        top: cap_top,
        right,
        bottom: cap_bottom,
    };
    FillRect(dc, &whole, track);
    let line = cap_bottom - (fill.clamp(0.0, 1.0) * (cap_bottom - cap_top) as f32) as i32;
    let filled = RECT {
        left,
        top: line,
        right,
        bottom: cap_bottom,
    };
    FillRect(dc, &filled, lit);
    // A null region means "no clip": the rest of the panel paints normally.
    SelectClipRgn(dc, std::ptr::null_mut());
    DeleteObject(capsule);
    DeleteObject(track);
    DeleteObject(lit);

    // The stand, dim, so the shape reads as a microphone at zero as well.
    let stand = CreateSolidBrush(rgb(58, 60, 68));
    FillRect(
        dc,
        &RECT {
            left: cx - 2,
            top: cap_bottom + 2,
            right: cx + 2,
            bottom: cap_bottom + 11,
        },
        stand,
    );
    FillRect(
        dc,
        &RECT {
            left: cx - 12,
            top: cap_bottom + 11,
            right: cx + 12,
            bottom: cap_bottom + 14,
        },
        stand,
    );
    DeleteObject(stand);

    EndPaint(window, &paint);
}

fn rgb(red: u32, green: u32, blue: u32) -> u32 {
    red | (green << 8) | (blue << 16)
}

fn wide(text: &str) -> Vec<u16> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    utf16
}
