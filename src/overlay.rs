//! The listening popup.
//!
//! A small toast in the bottom-right corner, where Windows notifications live:
//! a dark card, the word "Listening", a timer and a scrolling level meter. It is
//! created with `WS_EX_NOACTIVATE` and shown with `SW_SHOWNOACTIVATE`; the
//! daemon must never take focus, or the transcript would paste into the wrong
//! window.
//!
//! The meter is fed by `Status::level`, which the daemon samples from the live
//! capture every 100 ms. The window itself is plain GDI: one fill, one font, a
//! row of rectangles. There is no renderer to keep warm for a surface that is
//! visible for a few seconds at a time.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, Ellipse, EndPaint,
    FillRect, GetStockObject, InvalidateRect, SelectObject, SetBkMode, SetTextAlign, SetTextColor,
    SetWindowRgn, TextOutW, CLEARTYPE_QUALITY, DEFAULT_CHARSET, HGDIOBJ, NULL_PEN, PAINTSTRUCT,
    TA_RIGHT, TA_TOP, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, PeekMessageW,
    PostQuitMessage, RegisterClassW, SetWindowPos, ShowWindow, SystemParametersInfoW,
    TranslateMessage, HWND_TOPMOST, MSG, PM_REMOVE, SPI_GETWORKAREA, SWP_NOACTIVATE,
    SWP_NOOWNERZORDER, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WM_DESTROY, WM_ERASEBKGND, WM_PAINT,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::daemon::{Phase, Status};

const WIDTH: i32 = 340;
const HEIGHT: i32 = 92;
const MARGIN: i32 = 16;
const BARS: usize = 44;
const TICK: Duration = Duration::from_millis(100);

/// The level history and timer the window procedure draws. The run loop owns
/// the writes; `WM_PAINT` only reads a copy under the lock.
struct Meter {
    levels: [f32; BARS],
    seconds: f32,
}

static METER: OnceLock<Mutex<Meter>> = OnceLock::new();

fn meter() -> &'static Mutex<Meter> {
    METER.get_or_init(|| {
        Mutex::new(Meter {
            levels: [0.0; BARS],
            seconds: 0.0,
        })
    })
}

/// Add the popup and poll the daemon's status from its own thread.
pub fn spawn(status: Arc<Mutex<Status>>) -> Result<()> {
    std::thread::Builder::new()
        .name("overlay".into())
        .spawn(move || {
            if let Err(error) = run(status) {
                crate::log::write(&format!("overlay: {error:#}"));
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

        let mut levels = [0.0f32; BARS];
        let mut shown = false;
        let mut message: MSG = std::mem::zeroed();

        loop {
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }

            let (recording, level, seconds) = match status.lock() {
                Ok(status) => (
                    status.phase == Phase::Recording,
                    status.level,
                    status.seconds,
                ),
                Err(_) => (false, 0.0, 0.0),
            };

            if recording {
                if !shown {
                    levels.fill(0.0);
                    place(window);
                    ShowWindow(window, SW_SHOWNOACTIVATE);
                    shown = true;
                }
                levels.rotate_left(1);
                levels[BARS - 1] = level;
                if let Ok(mut meter) = meter().lock() {
                    meter.levels = levels;
                    meter.seconds = seconds;
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

/// Put the card above the taskbar in the bottom-right corner, where the clock
/// and notification centre are. Recomputed on every show: the taskbar can move.
unsafe fn place(window: HWND) {
    let mut area: RECT = std::mem::zeroed();
    let found = SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        &mut area as *mut RECT as *mut std::ffi::c_void,
        0,
    );
    if found == 0 {
        return;
    }
    let x = (area.right - WIDTH - MARGIN).max(area.left);
    let y = (area.bottom - HEIGHT - MARGIN).max(area.top);
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

    let (levels, seconds) = match meter().lock() {
        Ok(meter) => (meter.levels, meter.seconds),
        Err(_) => ([0.0; BARS], 0.0),
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

    let label = wide("Listening");
    TextOutW(dc, 32, 14, label.as_ptr(), label.len() as i32 - 1);

    SetTextAlign(dc, TA_RIGHT | TA_TOP);
    SetTextColor(dc, rgb(154, 160, 171));
    let timer = wide(&format!("{seconds:.1}s"));
    TextOutW(dc, width - 18, 16, timer.as_ptr(), timer.len() as i32 - 1);
    SetTextAlign(dc, TA_TOP);

    SelectObject(dc, previous);
    DeleteObject(font);

    // A symmetric meter: each column grows above and below the centre line, so
    // it reads as a waveform rather than a level bar.
    let left = 18;
    let right = width - 18;
    let gap = 3;
    let bar = (((right - left) - gap * (BARS as i32 - 1)) / BARS as i32).max(2);
    let top = 46;
    let bottom = rect.bottom - 14;
    let centre = (top + bottom) / 2;
    let reach = ((bottom - top) / 2 - 3).max(2) as f32;

    let brush = CreateSolidBrush(rgb(74, 222, 128));
    for (index, level) in levels.iter().enumerate() {
        let amplitude = level.clamp(0.0, 1.0).sqrt();
        let half = (2.0 + amplitude * reach) as i32;
        let x = left + index as i32 * (bar + gap);
        let column = RECT {
            left: x,
            top: centre - half,
            right: x + bar,
            bottom: centre + half,
        };
        FillRect(dc, &column, brush);
    }
    DeleteObject(brush);

    // A green dot before the label, drawn last so the fill cannot cover it.
    let dot_brush = CreateSolidBrush(rgb(74, 222, 128));
    let previous_brush = SelectObject(dc, dot_brush);
    let previous_pen = SelectObject(dc, GetStockObject(NULL_PEN));
    Ellipse(dc, 18, 17, 26, 25);
    SelectObject(dc, previous_pen);
    SelectObject(dc, previous_brush);
    DeleteObject(dot_brush);

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
