//! The settings window: the one visible surface in an app that is otherwise
//! invisible.
//!
//! It edits `config.toml` and exits. It never talks to the running daemon, so the
//! daemon does not need a reload protocol and cannot be wedged by a window; the
//! next capture simply reads the file it wrote.

use hippo_ui::rows::{settings_divider, settings_row, switch_row};
use hippo_ui::styles::*;
use hippo_ui::tokens::{self, AppTheme, Scheme};
use hippo_voice::capture;
use hippo_voice::config::{Config, PasteMethod};
use hippo_voice::model;
use iced::widget::{button, column, container, pick_list, row, scrollable, text};
use iced::{Element, Length, Task};

const SYSTEM_DEFAULT: &str = "System default";
const CHUNKS: [&str; 4] = ["10 s", "15 s", "20 s", "30 s"];

fn main() -> iced::Result {
    iced::application("Hippo Voice Settings", Settings::update, Settings::view)
        .theme(|state: &Settings| state.mode.iced())
        .font(include_bytes!("../assets/bootstrap-icons.ttf").as_slice())
        .window_size((620.0, 620.0))
        .run_with(Settings::boot)
}

#[derive(Debug, Clone)]
enum Message {
    Device(String),
    Paste(PasteMethod),
    Theme(AppTheme),
    Chunk(&'static str),
    ToggleTones,
    ToggleHistory,
    ToggleTrailing,
    Save,
}

struct Settings {
    config: Config,
    mode: AppTheme,
    devices: Vec<String>,
    saved: bool,
    error: Option<String>,
    model_state: String,
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

        (
            Self {
                mode: AppTheme::System,
                devices,
                config,
                saved: false,
                error: None,
                model_state,
            },
            Task::none(),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        // Any edit invalidates the "Saved" note, so it never describes a file that
        // no longer matches what is on screen.
        if !matches!(message, Message::Save) {
            self.saved = false;
        }
        match message {
            Message::Device(name) => {
                self.config.device = (name != SYSTEM_DEFAULT).then_some(name);
            }
            Message::Paste(method) => self.config.paste = method,
            Message::Theme(mode) => self.mode = mode,
            Message::Chunk(label) => {
                self.config.chunk_seconds = label
                    .trim_end_matches(" s")
                    .parse()
                    .unwrap_or(self.config.chunk_seconds);
            }
            Message::ToggleTones => self.config.tones = !self.config.tones,
            Message::ToggleHistory => self.config.history = !self.config.history,
            Message::ToggleTrailing => self.config.trailing_space = !self.config.trailing_space,
            Message::Save => self.save(),
        }
        Task::none()
    }

    fn save(&mut self) {
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

        let device_value = self
            .config
            .device
            .clone()
            .unwrap_or_else(|| SYSTEM_DEFAULT.to_string());

        let mut body = column![
            row![
                text("Hippo Voice").size(tokens::TEXT_XL),
                iced::widget::horizontal_space(),
                pick_list(AppTheme::ALL.as_slice(), Some(self.mode), Message::Theme)
                    .style(pill_pick(scheme)),
            ]
            .spacing(tokens::SPACE_LG)
            .align_y(iced::Alignment::Center),
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
                "Store delivered transcripts on disk, pruned after the retention period",
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

        if let Some(error) = &self.error {
            body = body.push(
                text(error.clone())
                    .size(tokens::TEXT_SM)
                    .color(scheme.danger),
            );
        }

        let saved = self
            .saved
            .then(|| text("Saved").size(tokens::TEXT_SM).color(scheme.green));

        let mut footer = row![iced::widget::horizontal_space()].width(Length::Fill);
        if let Some(saved) = saved {
            footer = footer.push(saved).spacing(tokens::SPACE_MD);
        }
        footer = footer.push(
            button(text("Save").size(16.0))
                .on_press(Message::Save)
                .style(color_button(scheme.blue, scheme.on_accent))
                .height(Length::Fixed(tokens::CONTROL_HEIGHT))
                .padding([0, 14]),
        );

        // The toggle rows carry a constant message, so the state they set has to
        // be inverted at the call site; this keeps them one shape.
        let page = column![body, footer.spacing(tokens::SPACE_MD)]
            .spacing(tokens::SPACE_2XL)
            .width(Length::Fill);

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
}

/// The stored seconds as one of the labels the pick list offers.
fn chunk_label(seconds: f32) -> &'static str {
    CHUNKS
        .into_iter()
        .find(|label| label.trim_end_matches(" s").parse::<f32>() == Ok(seconds))
        .unwrap_or("20 s")
}

fn section<'a>(label: &'a str, scheme: Scheme) -> Element<'a, Message> {
    text(label)
        .size(tokens::TEXT_2XS)
        .color(scheme.muted)
        .into()
}
