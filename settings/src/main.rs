//! The visible surface of an otherwise invisible app: settings and history.
//!
//! It reads `config.toml` and `history.tsv` and writes only the former; it never
//! talks to the running daemon, so a window cannot wedge a recording. The daemon
//! watches the file and applies what it finds on its next capture, which is why
//! there is no reload protocol to keep in step.

use hippo_dictate::capture;
use hippo_dictate::chord::Chord;
use hippo_dictate::config::{Clipboard, Config, PasteMethod};
use hippo_dictate::history;
use hippo_dictate::model;
use hippo_dictate::text as rules;
use hippo_ui::rows::{settings_divider, settings_row, switch_row};
use hippo_ui::styles::*;
use hippo_ui::tokens::{self, AppTheme, Scheme};
use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_input};
use iced::{font, Element, Font, Length, Task};
use std::sync::OnceLock;
use std::time::Duration;

mod instance;

const SYSTEM_DEFAULT: &str = "System default";
const CHUNKS: [&str; 4] = ["10 s", "15 s", "20 s", "30 s"];
/// Fallback phrase length for streaming when the online model is absent.
const STREAM_CHUNKS: [&str; 5] = ["1.0 s", "1.5 s", "2.0 s", "3.0 s", "5.0 s"];
const STREAM_SEGMENTS: [&str; 4] = ["5 s", "8 s", "12 s", "20 s"];

/// The header mark, in logical pixels, matching the size Hippo Transcribe uses so
/// the two windows read as one family. The source is 128 px, which is sharp at
/// 1x and marginal at 2x.
const HEADER_LOGO_SIZE: f32 = 67.0;

fn main() -> iced::Result {
    // The tray starts this with CREATE_NO_WINDOW, so an iced failure would
    // otherwise disappear with the console.
    let result = run();
    if let Err(error) = &result {
        hippo_dictate::log::error(&format!("settings: {error}"));
    }
    result
}

fn run() -> iced::Result {
    // The tray icon and the launcher start this executable on every click. The
    // second one hands its pane to the first and exits; the request reaches the
    // open window through the file its subscription polls.
    if !instance::claim(requested_pane().arg()) {
        return Ok(());
    }
    // Windows already had this as a named mutex, and it also needs the window
    // found and raised, which is what focus_running_window does.
    #[cfg(windows)]
    if focus_running_window() {
        return Ok(());
    }

    // `window` replaces `window_size`, so the size is carried in the same
    // settings that add the icon.
    let mut window = iced::window::Settings {
        size: iced::Size::new(700.0, 640.0),
        icon: window_icon(),
        ..iced::window::Settings::default()
    };
    #[cfg(target_os = "linux")]
    {
        // The compositor names the window by this, which is what the sway rule in
        // the single-instance handshake matches, and what a user's own rule should
        // match. Left empty, `[app_id=...]` matches nothing and the raise is a
        // no-op that looks like a bug in the handshake.
        window.platform_specific.application_id = "hippo-dictate-settings".to_string();
    }

    iced::application("Hippo Dictate", Settings::update, Settings::view)
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

impl Pane {
    /// The name the request file and the handshake carry.
    fn arg(self) -> &'static str {
        match self {
            Pane::Settings => "settings",
            Pane::History => "history",
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    Pane(Pane),
    Device(String),
    Paste(PasteMethod),
    Hotkey(String),
    StreamHotkey(String),
    StreamChunk(&'static str),
    StreamSegment(&'static str),
    ToggleStreamOnline,
    Theme(AppTheme),
    Chunk(&'static str),
    ToggleTones,
    ToggleHistory,
    ToggleTrailing,
    ToggleCleanup,
    ToggleFormatting,
    TogglePunctuation,
    ToggleCommands,
    Clipboard(Clipboard),
    Preview(String),
    HistoryFilter(String),
    SnippetTrigger(usize, String),
    SnippetText(usize, String),
    AddSnippet,
    RemoveSnippet(usize),
    ReplacementFrom(usize, String),
    ReplacementTo(usize, String),
    AddReplacement,
    RemoveReplacement(usize),
    Refresh,
    /// A pane request arrived from another instance.
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
    /// A phrase the rules are run on as you type, so the switches are not blind.
    preview: String,
    /// What the history pane is filtered to.
    history_filter: String,
    model_state: String,
    hotkey_draft: String,
    hotkey_error: Option<String>,
    stream_draft: String,
    stream_error: Option<String>,
}

impl Settings {
    fn boot() -> (Self, Task<Message>) {
        // A file that will not parse is shown as defaults, but the reason is kept:
        // the settings on screen are not the ones on disk, and saving would make
        // that true.
        let (config, config_error) = match Config::load() {
            Ok(config) => (config, None),
            Err(error) => (
                Config::default(),
                Some(format!(
                    "config.toml does not parse, so nothing can be saved here until it does: {error}"
                )),
            ),
        };
        let mut devices = vec![SYSTEM_DEFAULT.to_string()];
        devices.extend(capture::input_devices().unwrap_or_default());

        // Presence only, not hashes: boot must not block the first frame on 640 MB
        // of SHA-256. `hippo-dictate model verify` is the command that hashes.
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
        let pane = take_requested_pane().unwrap_or_else(requested_pane);
        let hotkey_error = Chord::parse(&config.hotkey)
            .err()
            .map(|error| error.to_string());
        let stream_error = Chord::parse(&config.stream_hotkey)
            .err()
            .map(|error| error.to_string());

        (
            Self {
                mode: AppTheme::System,
                hotkey_draft: config.hotkey.clone(),
                hotkey_error,
                stream_draft: config.stream_hotkey.clone(),
                stream_error,
                config,
                pane,
                devices,
                entries,
                copied: None,
                saved: false,
                preview: "um, mail sean at sign example dot com".to_string(),
                history_filter: String::new(),
                error: config_error,
                model_state,
            },
            Task::none(),
        )
    }

    fn subscription(_state: &Self) -> iced::Subscription<Message> {
        // A second instance writes the request file and exits, so the open window
        // polls for it. Cheap: one `stat` and one read every 400 ms, and only
        // while a window is open at all.
        iced::time::every(Duration::from_millis(400)).map(|_| Message::PollRequest)
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
            Message::StreamHotkey(text) => {
                self.stream_draft = text.clone();
                match Chord::parse(&text) {
                    Ok(_) => {
                        self.config.stream_hotkey = text;
                        self.stream_error = None;
                    }
                    Err(error) => self.stream_error = Some(error.to_string()),
                }
            }
            Message::StreamChunk(label) => {
                self.config.stream_seconds = label
                    .trim_end_matches(" s")
                    .parse()
                    .unwrap_or(self.config.stream_seconds);
            }
            Message::StreamSegment(label) => {
                self.config.stream_segment_seconds = label
                    .trim_end_matches(" s")
                    .parse()
                    .unwrap_or(self.config.stream_segment_seconds);
            }
            Message::ToggleStreamOnline => {
                self.config.stream_online = !self.config.stream_online;
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
            Message::ToggleCleanup => self.config.cleanup = !self.config.cleanup,
            Message::ToggleFormatting => self.config.formatting = !self.config.formatting,
            Message::TogglePunctuation => {
                self.config.punctuation_commands = !self.config.punctuation_commands
            }
            Message::ToggleCommands => self.config.commands = !self.config.commands,
            Message::Clipboard(kind) => self.config.clipboard = kind,
            Message::Preview(text) => self.preview = text,
            Message::HistoryFilter(text) => self.history_filter = text,
            Message::SnippetTrigger(index, trigger) => {
                if let Some(snippet) = self.config.snippets.get_mut(index) {
                    snippet.trigger = trigger;
                }
            }
            Message::SnippetText(index, text) => {
                if let Some(snippet) = self.config.snippets.get_mut(index) {
                    snippet.text = text;
                }
            }
            Message::AddSnippet => self.config.snippets.push(hippo_dictate::config::Snippet {
                trigger: String::new(),
                text: String::new(),
            }),
            Message::RemoveSnippet(index) => {
                if index < self.config.snippets.len() {
                    self.config.snippets.remove(index);
                }
            }
            Message::ReplacementFrom(index, from) => {
                if let Some(rule) = self.config.replacements.get_mut(index) {
                    rule.from = from;
                }
            }
            Message::ReplacementTo(index, to) => {
                if let Some(rule) = self.config.replacements.get_mut(index) {
                    rule.to = to;
                }
            }
            Message::AddReplacement => {
                self.config
                    .replacements
                    .push(hippo_dictate::config::Replacement {
                        from: String::new(),
                        to: String::new(),
                    })
            }
            Message::RemoveReplacement(index) => {
                if index < self.config.replacements.len() {
                    self.config.replacements.remove(index);
                }
            }
            Message::Refresh => self.entries = load_entries(&self.config),
            Message::PollRequest => {
                if let Some(pane) = take_requested_pane() {
                    self.pane = pane;
                }
            }
            Message::Copy(index) => {
                if let Some(entry) = self.entries.get(index) {
                    match hippo_dictate::inject::copy(&entry.text) {
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
        if let Some(error) = &self.stream_error {
            self.error = Some(format!("Streaming hotkey: {error}"));
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
        match write_config(&path, &pruned(&self.config)) {
            Ok(()) => {
                self.saved = true;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let scheme = self.mode.scheme();

        let header = row![
            iced::widget::image(header_logo())
                .width(Length::Fixed(HEADER_LOGO_SIZE))
                .height(Length::Fixed(HEADER_LOGO_SIZE)),
            column![
                text("Hippo Dictate").size(tokens::TEXT_XL).font(Font {
                    weight: font::Weight::Bold,
                    ..Font::DEFAULT
                }),
                text("HIPAA Inspired, Private Offline Dictation")
                    .size(tokens::TEXT_XS)
                    .color(scheme.muted),
            ]
            .spacing(tokens::SPACE_2XS),
            iced::widget::horizontal_space(),
            pick_list(AppTheme::ALL.as_slice(), Some(self.mode), Message::Theme)
                .style(pill_pick(scheme)),
        ]
        .spacing(tokens::SPACE_SM)
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
            page = page.push(
                text("Saved. Everything except the hotkeys applies on the next capture.")
                    .size(tokens::TEXT_SM)
                    .color(scheme.green),
            );
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
        let preview = match rules::apply(&self.preview, &self.config) {
            rules::Outcome::Text(text) => text,
            rules::Outcome::Command(command) => format!("command: {}", command.arg()),
        };
        let preview = if self.preview.trim().is_empty() {
            "Type a phrase to see what the rules do.".to_string()
        } else {
            format!("Result: {preview}")
        };
        let mut snippets = column![].spacing(tokens::SPACE_2XS).width(Length::Fill);
        for (index, snippet) in self.config.snippets.iter().enumerate() {
            snippets = snippets.push(
                row![
                    text_input("trigger", snippet.trigger.as_str())
                        .on_input(move |text| Message::SnippetTrigger(index, text))
                        .style(pill_input)
                        .width(Length::FillPortion(2)),
                    text_input("becomes this", snippet.text.as_str())
                        .on_input(move |text| Message::SnippetText(index, text))
                        .style(pill_input)
                        .width(Length::FillPortion(3)),
                    button(text("\u{2715}").size(tokens::TEXT_SM))
                        .on_press(Message::RemoveSnippet(index))
                        .style(segment_button(scheme, false, false, false)),
                ]
                .spacing(tokens::SPACE_SM)
                .align_y(iced::Alignment::Center),
            );
        }
        let mut replacements = column![].spacing(tokens::SPACE_2XS).width(Length::Fill);
        for (index, rule) in self.config.replacements.iter().enumerate() {
            replacements = replacements.push(
                row![
                    text_input("heard", rule.from.as_str())
                        .on_input(move |text| Message::ReplacementFrom(index, text))
                        .style(pill_input)
                        .width(Length::FillPortion(2)),
                    text_input("written", rule.to.as_str())
                        .on_input(move |text| Message::ReplacementTo(index, text))
                        .style(pill_input)
                        .width(Length::FillPortion(3)),
                    button(text("\u{2715}").size(tokens::TEXT_SM))
                        .on_press(Message::RemoveReplacement(index))
                        .style(segment_button(scheme, false, false, false)),
                ]
                .spacing(tokens::SPACE_SM)
                .align_y(iced::Alignment::Center),
            );
        }
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
            section("Text", scheme),
            switch_row(
                "Cleanup",
                "Strip fillers and repeated words, and tidy the spacing",
                self.config.cleanup,
                true,
                Message::ToggleCleanup,
                scheme,
            ),
            switch_row(
                "Formatting",
                "Turn \"at sign\" and \"dot com\" into an address",
                self.config.formatting,
                true,
                Message::ToggleFormatting,
                scheme,
            ),
            switch_row(
                "Spoken punctuation",
                "Treat \"comma\" and \"new paragraph\" as punctuation. Never an Enter key",
                self.config.punctuation_commands,
                true,
                Message::TogglePunctuation,
                scheme,
            ),
            switch_row(
                "Voice commands",
                "\"scratch that\" discards the utterance, \"quit hippo\" stops the daemon",
                self.config.commands,
                true,
                Message::ToggleCommands,
                scheme,
            ),
            settings_row(
                "Clipboard after",
                pick_list(
                    Clipboard::ALL.as_slice(),
                    Some(self.config.clipboard),
                    Message::Clipboard,
                )
                .style(pill_pick(scheme)),
                scheme,
            ),
            settings_row(
                "Try a phrase",
                text_input("type a phrase", self.preview.as_str())
                    .on_input(Message::Preview)
                    .style(pill_input)
                    .width(Length::Fill),
                scheme,
            ),
            text(preview).size(tokens::TEXT_XS).color(scheme.muted),
            settings_divider(scheme),
            section("Snippets", scheme),
            text("Say the trigger as the whole utterance and the text replaces it.")
                .size(tokens::TEXT_XS)
                .color(scheme.muted),
            snippets,
            button(text("Add snippet").size(tokens::TEXT_SM))
                .on_press(Message::AddSnippet)
                .style(segment_button(scheme, false, false, false)),
            settings_divider(scheme),
            section("Replacements", scheme),
            text("Whole words, case-insensitive, applied after everything else.")
                .size(tokens::TEXT_XS)
                .color(scheme.muted),
            replacements,
            button(text("Add replacement").size(tokens::TEXT_SM))
                .on_press(Message::AddReplacement)
                .style(segment_button(scheme, false, false, false)),
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
            settings_row(
                "Streaming hotkey",
                text_input("ctrl+shift+space", self.stream_draft.as_str())
                    .on_input(Message::StreamHotkey)
                    .style(pill_input)
                    .width(Length::Fixed(180.0)),
                scheme,
            ),
            text(match &self.stream_error {
                Some(error) => format!("Not a usable chord: {error}"),
                None => "Streams phrases while you speak instead of waiting for the stop press.".to_string(),
            })
            .size(tokens::TEXT_XS)
            .color(if self.stream_error.is_some() {
                scheme.danger
            } else {
                scheme.muted
            }),
            settings_row(
                "Phrase chunk",
                pick_list(
                    STREAM_CHUNKS.as_slice(),
                    Some(stream_chunk_label(self.config.stream_seconds)),
                    Message::StreamChunk
                )
                .style(pill_pick(scheme)),
                scheme,
            ),
            text("Fallback phrase length when the Pro word-by-word engine is off or unavailable. Shorter chunks appear sooner but cut words more often.")
                .size(tokens::TEXT_XS)
                .color(scheme.muted),
            settings_row(
                "Segment handoff",
                pick_list(
                    STREAM_SEGMENTS.as_slice(),
                    Some(segment_label(self.config.stream_segment_seconds)),
                    Message::StreamSegment
                )
                .style(pill_pick(scheme)),
                scheme,
            ),
            text("Word-by-word mode starts a fresh head after this long without a pause. The transcribed words stay in the document; this only bounds the recognizer's context. Shorter is safer, longer keeps more context.")
                .size(tokens::TEXT_XS)
                .color(scheme.muted),
            switch_row(
                "Word-by-word model",
                "Pro: use the optional 73 MB streaming zipformer for streaming mode. Off keeps everything on Parakeet, which is more accurate and multilingual.",
                self.config.stream_online,
                true,
                Message::ToggleStreamOnline,
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

        let needle = self.history_filter.trim().to_lowercase();
        let visible: Vec<(usize, &hippo_dictate::history::Entry)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| needle.is_empty() || entry.text.to_lowercase().contains(&needle))
            .collect();

        let filter: Element<'_, Message> =
            text_input("search the transcripts", self.history_filter.as_str())
                .on_input(Message::HistoryFilter)
                .style(pill_input)
                .width(Length::Fill)
                .into();

        let mut list = column![].spacing(tokens::SPACE_SM).width(Length::Fill);
        if self.entries.is_empty() {
            list = list.push(
                text("Nothing dictated yet, or everything is older than the retention window.")
                    .size(tokens::TEXT_SM)
                    .color(scheme.muted),
            );
        }
        if !self.entries.is_empty() && visible.is_empty() {
            list = list.push(
                text("No entry contains that.")
                    .size(tokens::TEXT_SM)
                    .color(scheme.muted),
            );
        }
        for (index, entry) in visible.into_iter().rev() {
            let copied = self.copied == Some(index);
            list = list.push(
                container(
                    row![
                        column![
                            text(hippo_dictate::log::stamp(entry.epoch))
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
            filter,
            list,
        ]
        .spacing(tokens::SPACE_MD)
        .width(Length::Fill)
        .into()
    }
}

/// Read and delete the pane the tray asked for, if any.
/// The pane this process was asked for, from the environment a launcher sets.
fn requested_pane() -> Pane {
    match std::env::var("HIPPO_DICTATE_START_PANE").as_deref() {
        Ok("history") => Pane::History,
        _ => Pane::Settings,
    }
}

fn take_requested_pane() -> Option<Pane> {
    let path = hippo_dictate::dirs::settings_request_file().ok()?;
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
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    let name: Vec<u16> = "Local\\hippo-dictate-settings\0".encode_utf16().collect();
    let title: Vec<u16> = "Hippo Dictate\0".encode_utf16().collect();

    unsafe {
        // CreateMutexW does not clear the last error on success, so a stale
        // ERROR_ALREADY_EXISTS from earlier startup work would look like a
        // second instance and close the first window before it opens.
        SetLastError(0);
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

fn stream_chunk_label(seconds: f32) -> &'static str {
    STREAM_CHUNKS
        .into_iter()
        .find(|label| label.trim_end_matches(" s").parse::<f32>() == Ok(seconds))
        .unwrap_or("3.0 s")
}

fn segment_label(seconds: f32) -> &'static str {
    STREAM_SEGMENTS
        .into_iter()
        .find(|label| label.trim_end_matches(" s").parse::<f32>() == Ok(seconds))
        .unwrap_or("8 s")
}

fn load_entries(config: &Config) -> Vec<history::Entry> {
    if !config.history {
        return Vec::new();
    }
    history::recent(config.history_days).unwrap_or_default()
}

/// Write the settings, keeping whatever this window does not know about.
///
/// `Config` has no field for a key a future version adds, and serialising it over
/// the file would delete that key. Editing the document in place touches only the
/// fields this window owns, and leaves the rest of the file — including the
/// comments around keys it never touches — as the user wrote it.
///
/// `device` is the one field whose "unset" state has to be written as an absence:
/// `None` means the host default, so the key is removed rather than left behind.
/// A row the user opened and left blank is not a rule: writing it out would leave
/// dead entries in the file for every abandoned "Add".
fn pruned(config: &Config) -> Config {
    let mut config = config.clone();
    config
        .snippets
        .retain(|snippet| !snippet.trigger.trim().is_empty());
    config
        .replacements
        .retain(|rule| !rule.from.trim().is_empty());
    config
}

fn write_config(path: &std::path::Path, config: &Config) -> Result<(), String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    // A file that will not parse is left alone. The window is showing defaults
    // because it could not read the real settings, so saving would replace a
    // hand-edit with them — destroying exactly the text the daemon is refusing to
    // use. The check is against `Config`, not the TOML grammar: `paste =
    // "nonsense"` is valid TOML and an unreadable setting.
    if !existing.trim().is_empty() {
        if let Err(error) = toml::from_str::<Config>(&existing) {
            return Err(format!(
                "config.toml was not overwritten because it does not parse ({error}); fix it in an editor"
            ));
        }
    }
    let mut document = existing
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_else(|_| toml_edit::DocumentMut::new());

    let ours: toml_edit::DocumentMut = toml::to_string_pretty(config)
        .map_err(|error| error.to_string())?
        .parse()
        .map_err(|error: toml_edit::TomlError| error.to_string())?;
    for (key, item) in ours.iter() {
        document[key] = item.clone();
    }
    if config.device.is_none() {
        document.remove("device");
    }

    std::fs::write(path, document.to_string()).map_err(|error| error.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_keeps_what_this_window_does_not_own() {
        let path = std::env::temp_dir().join(format!(
            "hippo-dictate-settings-{}.toml",
            std::process::id()
        ));
        // A comment above a key the window never writes, and a key from a version
        // that has not been released yet: both have to survive a save.
        std::fs::write(
            &path,
            "# hand edit\nfuture_key = 1\npaste = \"auto\"\ndevice = \"USB Mic\"\n",
        )
        .expect("write the fixture");

        let config = Config {
            paste: PasteMethod::Ydotool,
            ..Config::default()
        };
        write_config(&path, &config).expect("save");

        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(
            text.contains("# hand edit"),
            "a comment was dropped: {text}"
        );
        assert!(
            text.contains("future_key = 1"),
            "an unknown key was dropped: {text}"
        );
        assert!(
            text.contains("paste = \"ydotool\""),
            "the edited value was not written: {text}"
        );
        assert!(
            !text.contains("device ="),
            "the host default was selected but the old device stayed in the file: {text}"
        );

        let back: Config = toml::from_str(&text).expect("parse the saved file");
        assert_eq!(back.paste, PasteMethod::Ydotool);
        assert_eq!(back.device, None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn blank_rows_are_not_written_out() {
        let mut config = Config::default();
        config.snippets = vec![
            hippo_dictate::config::Snippet {
                trigger: "link".to_string(),
                text: "https://example.invalid".to_string(),
            },
            hippo_dictate::config::Snippet {
                trigger: "  ".to_string(),
                text: "abandoned".to_string(),
            },
        ];
        config.replacements = vec![hippo_dictate::config::Replacement {
            from: String::new(),
            to: "nothing".to_string(),
        }];

        let kept = pruned(&config);
        assert_eq!(kept.snippets.len(), 1);
        assert_eq!(kept.snippets[0].trigger, "link");
        assert!(kept.replacements.is_empty());
    }

    #[test]
    fn a_file_that_does_not_parse_is_never_overwritten() {
        let path =
            std::env::temp_dir().join(format!("hippo-dictate-broken-{}.toml", std::process::id()));
        // A typo the user can still fix by hand: saving over it would replace
        // their file with the defaults this window fell back to.
        std::fs::write(&path, "cleanup = true\npaste = \"nonsense\"\n").expect("write");

        let error = write_config(&path, &Config::default()).expect_err("it overwrote the file");
        assert!(error.contains("does not parse"), "{error}");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("nonsense"), "the file was rewritten: {text}");

        let _ = std::fs::remove_file(&path);
    }
}
