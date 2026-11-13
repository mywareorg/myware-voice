//! The visible surface of an otherwise invisible app: settings and history.
//!
//! It reads `config.toml` and `history.tsv` and writes only the former; it never
//! talks to the running daemon, so there is no reload protocol to keep in step and
//! a window cannot wedge a recording. The next capture reads the file it wrote.

use hippo_ui::rows::{settings_divider, settings_row, switch_row};
use hippo_ui::styles::*;
use hippo_ui::tokens::{self, AppTheme, Scheme};
use hippo_voice::capture;
use hippo_voice::chord::Chord;
use hippo_voice::config::{Config, PasteMethod};
use hippo_voice::history;
use hippo_voice::model;
use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_input};
use iced::{Element, Length, Task};
use std::sync::OnceLock;
#[cfg(windows)]
use std::time::Duration;

const SYSTEM_DEFAULT: &str = "System default";
const CHUNKS: [&str; 4] = ["10 s", "15 s", "20 s", "30 s"];

/// The header mark, in logical pixels. It sits inside the control height beside
/// the title, and 128 px of source cover a 4x display.
const HEADER_LOGO_SIZE: f32 = 28.0;

fn main() -> iced::Result {
    // The tray icon launches this executable on every click. Focus the window
    // that is already open instead of stacking another one; the pane request is
    // picked up by its subscription.
    #[cfg(windows)]
    if focus_running_window() {
        return Ok(());
    }

    // `window` replaces `window_size`, so the size is carried in the same
    // settings that add the icon.
    let window = iced::window::Settings {
        size: iced::Size::new(700.0, 640.0),
        icon: window_icon(),
        ..iced::window::Settings::default()
    };

    iced::application("Hippo Voice", Settings::update, Settings::view)
        .theme(|state: &Settings| state.mode.iced())
        .font(include_bytes!("../assets/bootstrap-icons.ttf").as_slice())
        .window(window)
        .subscription(Settings::subscription)
        .run_with(Settings::boot)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Settings,
    History,
}

#[derive(Debug, Clone)]
enum Message {
    Pane(Pane),
    Device(String),
    Paste(PasteMethod),
    Hotkey(String),
    Theme(AppTheme),
    Chunk(&'static str),
    ToggleTones,
    ToggleHistory,
    ToggleTrailing,
    Refresh,
    PollRequest,
    Copy(usize),
    ClearHistory,
    Save,
}

struct Settings {
    config: Config,
    mode: AppTheme,
    pane: Pane,
    devices: Vec<String>,
    entries: Vec<history::Entry>,
    copied: Option<usize>,
    saved: bool,
    error: Option<String>,
    model_state: String,
    hotkey_draft: String,
    hotkey_error: Option<String>,
}

impl Settings {
    fn boot() -> (Self, Task<Message>) {
        let config = Config::load().unwrap_or_default();
        let mut devices = vec![SYSTEM_DEFAULT.to_string()];
        devices.extend(capture::input_devices().unwrap_or_default());

        // Presence only, not hashes: boot must not block the first frame on 640 MB
        // of SHA-256. `hippo-voice model verify` is the command that hashes.
        let model_state = match model::resolve_dir() {
            Ok(dir) => match model::inspect(&dir, false) {
                Ok(rows)
                    if rows
                        .iter()
                        .all(|(_, state, _)| *state == model::State::Present) =>
                {
                    format!("installed at {}", dir.display())
                }
                Ok(_) => format!("incomplete at {}", dir.display()),
                Err(error) => format!("unreadable: {error}"),
            },
            Err(error) => format!("missing: {error}"),
        };

        let entries = load_entries(&config);
        // The tray opens a specific pane; without a window manager there is no
        // way to pass an argument to an iced app, so the choice comes through
        // the environment. A request file covers a window that is already open.
        let pane = match std::env::var("HIPPO_VOICE_START_PANE").as_deref() {
            Ok("history") => Pane::History,
            _ => Pane::Settings,
        };
        let pane = take_requested_pane().unwrap_or(pane);
        let hotkey_error = Chord::parse(&config.hotkey)
            .err()
            .map(|error| error.to_string());

        (
            Self {
                mode: AppTheme::System,
                hotkey_draft: config.hotkey.clone(),
                hotkey_error,
                config,
                pane,
                devices,
                entries,
                copied: None,
                saved: false,
                error: None,
                model_state,
            },
            Task::none(),
        )
    }

    fn subscription(_state: &Self) -> iced::Subscription<Message> {
        #[cfg(windows)]
        {
            iced::time::every(Duration::from_millis(400)).map(|_| Message::PollRequest)
        }
        #[cfg(not(windows))]
        {
            iced::Subscription::none()
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        if !matches!(message, Message::Save) {
            self.saved = false;
        }
        self.copied = None;

        match message {
            Message::Pane(pane) => self.pane = pane,
            Message::Device(name) => {
                self.config.device = (name != SYSTEM_DEFAULT).then_some(name);
            }
            Message::Paste(method) => self.config.paste = method,
            Message::Hotkey(text) => {
                self.hotkey_draft = text.clone();
                match Chord::parse(&text) {
                    Ok(_) => {
                        self.config.hotkey = text;
                        self.hotkey_error = None;
                    }
                    Err(error) => self.hotkey_error = Some(error.to_string()),
                }
            }
            Message::Theme(mode) => self.mode = mode,
            Message::Chunk(label) => {
                self.config.chunk_seconds = label
                    .trim_end_matches(" s")
                    .parse()
                    .unwrap_or(self.config.chunk_seconds);
            }
            Message::ToggleTones => self.config.tones = !self.config.tones,
            Message::ToggleHistory => {
                self.config.history = !self.config.history;
                self.entries = load_entries(&self.config);
            }
            Message::ToggleTrailing => self.config.trailing_space = !self.config.trailing_space,
            Message::Refresh => self.entries = load_entries(&self.config),
            Message::PollRequest => {
                if let Some(pane) = take_requested_pane() {
                    self.pane = pane;
                }
            }
            Message::Copy(index) => {
                if let Some(entry) = self.entries.get(index) {
                    match hippo_voice::inject::copy(&entry.text) {
                        Ok(()) => self.copied = Some(index),
                        Err(error) => self.error = Some(format!("could not copy: {error}")),
                    }
                }
            }
            Message::ClearHistory => match history::clear() {
                Ok(()) => {
                    self.entries.clear();
                    self.error = None;
                }
                Err(error) => self.error = Some(error.to_string()),
            },
            Message::Save => self.save(),
        }
        Task::none()
    }

    fn save(&mut self) {
        if let Some(error) = &self.hotkey_error {
            self.error = Some(format!("Hotkey: {error}"));
            return;
        }
        let path = match Config::path() {
            Ok(path) => path,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        if let Some(dir) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(dir) {
                self.error = Some(error.to_string());
                return;
            }
        }
        match toml::to_string_pretty(&self.config) {
            Ok(text) => match std::fs::write(&path, text) {
                Ok(()) => {
                    self.saved = true;
                    self.error = None;
                }
                Err(error) => self.error = Some(error.to_string()),
            },
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let scheme = self.mode.scheme();

        let header = row![
            iced::widget::image(header_logo())
                .width(Length::Fixed(HEADER_LOGO_SIZE))
                .height(Length::Fixed(HEADER_LOGO_SIZE)),
            text("Hippo Voice").size(tokens::TEXT_XL),
            iced::widget::horizontal_space(),
            pick_list(AppTheme::ALL.as_slice(), Some(self.mode), Message::Theme)
                .style(pill_pick(scheme)),
        ]
        .spacing(tokens::SPACE_LG)
        .align_y(iced::Alignment::Center);

        let tabs = row![
            tab("Settings", Pane::Settings, self.pane, scheme),
            tab("History", Pane::History, self.pane, scheme),
        ]
        .spacing(tokens::SPACE_2XS);

        let body = match self.pane {
            Pane::Settings => self.settings_pane(scheme),
            Pane::History => self.history_pane(scheme),
        };

        let mut page = column![header, tabs, body]
            .spacing(tokens::SPACE_LG)
            .width(Length::Fill);

        if let Some(error) = &self.error {
            page = page.push(
                text(error.as_str())
                    .size(tokens::TEXT_SM)
                    .color(scheme.danger),
            );
        }
        if self.saved {
            page = page.push(text("Saved").size(tokens::TEXT_SM).color(scheme.green));
        }

        container(
            scrollable(
                container(page)
                    .padding(tokens::SPACE_2XL)
                    .width(Length::Fill),
            )
            .style(scrollbar_style)
            .height(Length::Fill),
        )
        .style(move |_theme| container::Style {
            background: Some(scheme.bg.into()),
            ..container::Style::default()
        })
        .into()
    }

    fn settings_pane(&self, scheme: Scheme) -> Element<'_, Message> {
        let device_value = self
            .config
            .device
            .clone()
            .unwrap_or_else(|| SYSTEM_DEFAULT.to_string());

        let mut body = column![
            text("Speak, and the transcript lands where you were typing.")
                .size(tokens::TEXT_SM)
                .color(scheme.muted),
            section("Input", scheme),
            settings_row(
                "Microphone",
                pick_list(self.devices.clone(), Some(device_value), Message::Device)
                    .style(pill_pick(scheme)),
                scheme,
            ),
            settings_divider(scheme),
            section("Delivery", scheme),
            settings_row(
                "Paste with",
                pick_list(
                    PasteMethod::ALL.as_slice(),
                    Some(self.config.paste),
                    Message::Paste
                )
                .style(pill_pick(scheme)),
                scheme,
            ),
            switch_row(
                "Trailing space",
                "Separate consecutive dictations instead of joining them",
                self.config.trailing_space,
                true,
                Message::ToggleTrailing,
                scheme,
            ),
            settings_divider(scheme),
            section("Activation", scheme),
            settings_row(
                "Hotkey (Windows)",
                text_input("ctrl+space", self.hotkey_draft.as_str())
                    .on_input(Message::Hotkey)
                    .style(pill_input)
                    .width(Length::Fixed(180.0)),
                scheme,
            ),
            text(match &self.hotkey_error {
                Some(error) => format!("Not a usable chord: {error}"),
                None => "The daemon owns this chord system-wide while it runs. Linux uses the compositor binding instead.".to_string(),
            })
            .size(tokens::TEXT_XS)
            .color(if self.hotkey_error.is_some() {
                scheme.danger
            } else {
                scheme.muted
            }),
            settings_divider(scheme),
            section("Recognition", scheme),
            settings_row(
                "Split long audio",
                pick_list(
                    CHUNKS.as_slice(),
                    Some(chunk_label(self.config.chunk_seconds)),
                    Message::Chunk
                )
                .style(pill_pick(scheme)),
                scheme,
            ),
            text(format!(
                "Parakeet degrades on one long pass, so a recording is cut at silences into chunks this long. Idle unload after {} minutes.",
                self.config.idle_unload_minutes
            ))
            .size(tokens::TEXT_XS)
            .color(scheme.muted),
            settings_divider(scheme),
            section("Feedback and privacy", scheme),
            switch_row(
                "Cue tones",
                "A rising note when recording starts, a falling one when text is delivered",
                self.config.tones,
                true,
                Message::ToggleTones,
                scheme,
            ),
            switch_row(
                "Keep history",
                "Store delivered transcripts on disk, so they can be reviewed here",
                self.config.history,
                true,
                Message::ToggleHistory,
                scheme,
            ),
            text(format!("Model: {}", self.model_state))
                .size(tokens::TEXT_XS)
                .color(scheme.muted),
        ]
        .spacing(tokens::SPACE_MD)
        .width(Length::Fill);

        let mut footer = row![iced::widget::horizontal_space()].width(Length::Fill);
        footer = footer.push(
            button(text("Save").size(16.0))
                .on_press(Message::Save)
                .style(color_button(scheme.blue, scheme.on_accent))
                .height(Length::Fixed(tokens::CONTROL_HEIGHT))
                .padding([0, 14]),
        );
        body = body.push(footer);
        body.into()
    }

    fn history_pane(&self, scheme: Scheme) -> Element<'_, Message> {
        if !self.config.history {
            return column![
                text("History is off.")
                    .size(tokens::TEXT_MD)
                    .color(scheme.text),
                text(
                    "Turn on \u{201c}Keep history\u{201d} in Settings and every delivered \
                     transcript is stored on this machine so it can be reviewed here. \
                     Nothing is sent anywhere."
                )
                .size(tokens::TEXT_SM)
                .color(scheme.muted),
                button(text("Open settings").size(16.0))
                    .on_press(Message::Pane(Pane::Settings))
                    .style(pill_button_secondary)
                    .height(Length::Fixed(tokens::CONTROL_HEIGHT))
                    .padding([0, 14]),
            ]
            .spacing(tokens::SPACE_LG)
            .into();
        }

        let mut list = column![].spacing(tokens::SPACE_SM).width(Length::Fill);
        if self.entries.is_empty() {
            list = list.push(
                text("Nothing dictated yet, or everything is older than the retention window.")
                    .size(tokens::TEXT_SM)
                    .color(scheme.muted),
            );
        }
        for (index, entry) in self.entries.iter().enumerate().rev() {
            let copied = self.copied == Some(index);
            list = list.push(
                container(
                    row![
                        column![
                            text(hippo_voice::log::stamp(entry.epoch))
                                .size(tokens::TEXT_2XS)
                                .color(scheme.muted),
                            text(entry.text.as_str()).size(tokens::TEXT_SM),
                        ]
                        .spacing(2)
                        .width(Length::Fill),
                        button(text(if copied { "Copied" } else { "Copy" }).size(tokens::TEXT_SM))
                            .on_press(Message::Copy(index))
                            .style(pill_button_secondary)
                            .height(Length::Fixed(tokens::CONTROL_HEIGHT))
                            .padding([0, 12]),
                    ]
                    .spacing(tokens::SPACE_LG)
                    .align_y(iced::Alignment::Center),
                )
                .width(Length::Fill)
                .padding([6, 8])
                .style(move |_theme| container::Style {
                    background: Some(scheme.panel_bg.into()),
                    border: island_border(&scheme),
                    ..container::Style::default()
                }),
            );
        }

        column![
            row![
                text(format!("{} entries", self.entries.len()))
                    .size(tokens::TEXT_XS)
                    .color(scheme.muted),
                iced::widget::horizontal_space(),
                button(text("Refresh").size(16.0))
                    .on_press(Message::Refresh)
                    .style(pill_button_secondary)
                    .height(Length::Fixed(tokens::CONTROL_HEIGHT))
                    .padding([0, 12]),
                button(text("Clear history").size(16.0))
                    .on_press(Message::ClearHistory)
                    .style(color_button(scheme.danger, scheme.on_accent))
                    .height(Length::Fixed(tokens::CONTROL_HEIGHT))
                    .padding([0, 12]),
            ]
            .spacing(tokens::SPACE_SM)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill),
            list,
        ]
        .spacing(tokens::SPACE_MD)
        .width(Length::Fill)
        .into()
    }
}

/// Read and delete the pane the tray asked for, if any.
fn take_requested_pane() -> Option<Pane> {
    let path = hippo_voice::dirs::settings_request_file().ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    match text.trim() {
        "history" => Some(Pane::History),
        "settings" => Some(Pane::Settings),
        _ => None,
    }
}

/// Focus the settings window that is already open, if there is one.
///
/// The mutex is the authority: a window that is still starting has no title yet,
/// but the request file makes it open the right pane when it does. The handle is
/// deliberately never closed, so it lives exactly as long as this process.
#[cfg(windows)]
fn focus_running_window() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    let name: Vec<u16> = "Local\\hippo-voice-settings\0".encode_utf16().collect();
    let title: Vec<u16> = "Hippo Voice\0".encode_utf16().collect();

    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            return false;
        }
        // A second process is the one that sees the existing mutex.
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return false;
        }

        // The first process may not have created its window yet.
        for _ in 0..80 {
            let window = FindWindowW(std::ptr::null(), title.as_ptr());
            if !window.is_null() {
                ShowWindow(window, SW_RESTORE);
                SetForegroundWindow(window);
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }
}

/// The stored seconds as one of the labels the pick list offers.
fn chunk_label(seconds: f32) -> &'static str {
    CHUNKS
        .into_iter()
        .find(|label| label.trim_end_matches(" s").parse::<f32>() == Ok(seconds))
        .unwrap_or("20 s")
}

fn load_entries(config: &Config) -> Vec<history::Entry> {
    if !config.history {
        return Vec::new();
    }
    history::recent(config.history_days).unwrap_or_default()
}

fn tab<'a>(label: &'a str, pane: Pane, current: Pane, scheme: Scheme) -> Element<'a, Message> {
    button(text(label).size(16.0))
        .padding([0, 12])
        .height(Length::Fixed(tokens::CONTROL_HEIGHT))
        .style(segment_button(scheme, current == pane, false, false))
        .on_press(Message::Pane(pane))
        .into()
}

fn section<'a>(label: &'a str, scheme: Scheme) -> Element<'a, Message> {
    text(label)
        .size(tokens::TEXT_2XS)
        .color(scheme.muted)
        .into()
}

/// The original hippo, downscaled by `scripts/make-app-icons.py`.
fn header_logo() -> iced::widget::image::Handle {
    static LOGO: OnceLock<iced::widget::image::Handle> = OnceLock::new();
    LOGO.get_or_init(|| {
        let image = image::load_from_memory(include_bytes!("../../icons/hippo.png"))
            .expect("bundled header mark")
            .into_rgba8();
        let (width, height) = image.dimensions();
        iced::widget::image::Handle::from_rgba(width, height, image.into_raw())
    })
    .clone()
}

/// Title-bar and taskbar icon. It is the same artwork `build.rs` embeds, so the
/// window, the taskbar and Explorer agree; `None` only if the PNG will not decode.
fn window_icon() -> Option<iced::window::Icon> {
    let image = image::load_from_memory(include_bytes!("../../icons/app-256.png"))
        .ok()?
        .into_rgba8();
    let (width, height) = image.dimensions();
    iced::window::icon::from_rgba(image.into_raw(), width, height).ok()
}
