//! The resident process: one microphone, one warm recognizer, one socket.
//!
//! `hippo-voice toggle` — what the compositor keybind runs — is a client of this
//! process. The daemon never takes focus, which is the only reason a paste can
//! land in the window the user was already typing in.
//!
//! Capture starts before the recognizer is asked to load. A cold load is ~1.5 s,
//! and the user is already talking by then, so the load is overlapped with the
//! first utterance rather than paid after it.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::audio;
use crate::capture::Recorder;
use crate::config::Config;
use crate::inject::{self, Focus};
use crate::log;
use crate::sherpa::Sherpa;
use crate::tone::{self, Cue};
use crate::transport::{self, Listener, Stream};

/// Longest pause between event-loop polls. Also the status freshness.
const TICK: Duration = Duration::from_millis(100);

/// Identifies the daemon on the wire, so a recycled port or a socket some other
/// service has taken over cannot be mistaken for one.
const APP: &str = "hippo-voice";
const PROTOCOL: u32 = 1;

/// A canned transcript, so the state machine can be tested with no model on
/// disk. Read once at startup; a spawned daemon inherits it.
/// Skip the clipboard and the paste, so a test does not need `wl-copy` or a
/// focused window. Delivery itself has its own tests.
fn fake_delivery() -> bool {
    std::env::var_os("HIPPO_VOICE_FAKE_DELIVERY").is_some()
}

fn fake_model() -> Option<String> {
    std::env::var("HIPPO_VOICE_FAKE_MODEL")
        .ok()
        .filter(|text| !text.is_empty())
}

/// Report a problem to the terminal and to the log file.
fn note(message: &str) {
    eprintln!("{message}");
    log::write(message);
}

/// A failure the user needs to report, not just a line in the rolling log.
fn note_error(message: &str) {
    eprintln!("{message}");
    log::error(message);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Idle,
    Recording,
    Decoding,
}

impl Phase {
    pub fn arg(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Recording => "recording",
            Phase::Decoding => "decoding",
        }
    }
}

/// What clients can see at any moment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub phase: Phase,
    pub seconds: f32,
    pub device: String,
    pub generation: u64,
    pub last_text: Option<String>,
    /// The chord in force, or why it is not. `None` while registering.
    pub hotkey: Option<String>,
    /// RMS of the live capture, for the listening popup. Zero when idle.
    #[serde(default)]
    pub level: f32,
    /// Normalised frequency-band magnitudes for the popup's meter.
    #[serde(default)]
    pub bands: Vec<f32>,
    /// Whether the current capture commits phrases while the user speaks.
    #[serde(default)]
    pub streaming: bool,
}

/// Columns in the popup's frequency meter.
pub const SPECTRUM_BANDS: usize = 36;

impl Default for Status {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            seconds: 0.0,
            device: String::new(),
            generation: 0,
            last_text: None,
            hotkey: None,
            level: 0.0,
            bands: Vec::new(),
            streaming: false,
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Toggle,
    /// Start or stop dictation that commits phrases while the user speaks.
    ToggleStream,
    /// Stop the daemon. Needed by the uninstaller and by anyone asking why a
    /// process is still running.
    Quit,
    Decoded {
        generation: u64,
        text: String,
    },
    /// One incrementally decoded phrase. `final_chunk` ends the session after
    /// its text is delivered.
    Streamed {
        generation: u64,
        text: String,
        final_chunk: bool,
    },
    Failed(String),
}

/// A request from a client.
#[derive(Debug, Deserialize)]
struct Request {
    cmd: String,
    /// Present on Windows, where a loopback port alone is not authority.
    #[serde(default)]
    token: Option<String>,
}

/// The reply to a client.
#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One request/response round trip.
pub fn request(command: &str) -> Result<Reply> {
    let mut stream = transport::connect()?;
    let mut body = serde_json::json!({ "cmd": command });
    if let Some(token) = transport::token() {
        body["token"] = serde_json::Value::String(token);
    }
    serde_json::to_writer(&mut stream, &body)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let reply: Reply = serde_json::from_str(&line).context("parse reply")?;
    // Something answered, but not us: do not adopt it, and do not start a second
    // daemon on top of it.
    if let Some(app) = &reply.app {
        if app != APP {
            anyhow::bail!("{app} is listening on the daemon endpoint, not {APP}");
        }
    }
    Ok(reply)
}

/// Send `toggle`, starting the daemon first if it is not running.
pub fn toggle() -> Result<Reply> {
    match request("toggle") {
        Ok(reply) => Ok(reply),
        // Only "nothing is listening" starts a daemon. A live daemon that
        // answered badly must not get a competitor.
        Err(error) if !transport::is_unreachable(&error) => Err(error),
        Err(_) => {
            spawn_daemon()?;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match request("toggle") {
                    Ok(reply) => return Ok(reply),
                    Err(error) if Instant::now() < deadline => {
                        let _ = error;
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(error) => return Err(error).context("daemon did not come up"),
                }
            }
        }
    }
}

fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("current executable")?;
    // Prefer the GUI-subsystem daemon binary when it is installed beside us: it
    // opens no console window, which matters at login. Fall back to this
    // executable's `daemon` subcommand for a development build.
    let sibling = exe.parent().map(|dir| {
        dir.join(if cfg!(windows) {
            "hippo-voice-daemon.exe"
        } else {
            "hippo-voice-daemon"
        })
    });
    let log = log::path()?;
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .ok();
    let mut command = match sibling.filter(|path| path.is_file()) {
        Some(path) => std::process::Command::new(path),
        None => {
            let mut command = std::process::Command::new(exe);
            command.arg("daemon");
            command
        }
    };
    command.stdin(std::process::Stdio::null());
    match out {
        Some(file) => {
            let err = file.try_clone()?;
            command.stdout(file).stderr(err);
        }
        None => {
            command.stdout(std::process::Stdio::null());
            command.stderr(std::process::Stdio::null());
        }
    }
    // Detach from the launcher so the process that started the daemon cannot
    // take it down: its own process group on Unix, no console on Windows.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().context("spawn daemon")?;
    Ok(())
}

/// What the decoder thread is asked to do.
enum DecodeJob {
    Warm,
    Decode {
        samples: Vec<f32>,
        generation: u64,
        chunk_seconds: f32,
    },
    /// Decode one phrase without ending the session. The samples are already
    /// resampled and normalised, and `final_chunk` asks for the session to end
    /// after delivery.
    Stream {
        samples: Vec<f32>,
        generation: u64,
        final_chunk: bool,
    },
    /// Drop the recognizer. ONNX Runtime does not return a long decode's arena
    /// to the OS, so holding the model forever is the expensive option.
    Unload,
}

struct Daemon {
    config: Config,
    jobs: Sender<DecodeJob>,
    status: Arc<Mutex<Status>>,
    recorder: Option<Recorder>,
    generation: Arc<AtomicU64>,
    unloaded: bool,
    last_activity: Instant,
    started: Instant,
    focus: Option<Focus>,
    fake_delivery: bool,
    /// Streaming dictation: whether the current capture commits phrases, how
    /// many raw mono samples have been committed, and the window that must stay
    /// focused for the whole session.
    streaming: bool,
    stream_committed: usize,
    stream_focus: Option<Focus>,
    stream_text: String,
    /// Word-by-word path: a streaming zipformer stream kept for the session.
    online: Option<crate::sherpa::OnlineStream>,
    online_fed: usize,
    online_segment_start: usize,
    online_last: String,
    online_pasted: String,
}

impl Daemon {
    fn handle(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Toggle => self.toggle(),
            Event::ToggleStream => self.toggle_stream(),
            // Handled by the run loop, which breaks out of it.
            Event::Quit => Ok(()),
            Event::Decoded { generation, text } => {
                let outcome = self.delivered(generation, text);
                // A failed copy or paste must not leave the daemon stuck in
                // Decoding: the next press would be read as "cancel", not "record".
                if outcome.is_err() {
                    self.set(Phase::Idle);
                }
                outcome
            }
            Event::Streamed {
                generation,
                text,
                final_chunk,
            } => {
                let outcome = self.streamed(generation, text, final_chunk);
                if outcome.is_err() {
                    self.set(Phase::Idle);
                }
                outcome
            }
            Event::Failed(error) => {
                note_error(&format!("failed: {error}"));
                self.stop_recording();
                self.set(Phase::Idle);
                Ok(())
            }
        }
    }

    fn toggle(&mut self) -> Result<()> {
        if self.streaming {
            return self.stop_streaming();
        }
        match self.status.lock().map(|s| s.phase).unwrap_or(Phase::Idle) {
            Phase::Recording => self.stop_recording_and_decode(),
            // A second press during decode means "I want to talk", not "wait".
            // Starting a capture bumps the generation, which abandons the decode.
            Phase::Decoding => self.start_recording(false),
            Phase::Idle => self.start_recording(false),
        }
    }

    /// Start streaming, or finish the session if one is already running.
    ///
    /// Pressing it during a normal recording adopts that capture: everything
    /// already spoken is treated as committed and is not pasted, so the switch
    /// does not dump a buffer of old text into the window.
    fn toggle_stream(&mut self) -> Result<()> {
        if self.streaming {
            return self.stop_streaming();
        }

        let phase = self.status.lock().map(|s| s.phase).unwrap_or(Phase::Idle);
        if phase == Phase::Recording {
            let committed = self
                .recorder
                .as_ref()
                .map(|recorder| recorder.snapshot_mono().0.len())
                .unwrap_or(0);
            self.streaming = true;
            self.stream_committed = committed;
            self.stream_focus = inject::focus();
            self.stream_text.clear();
            if self.config.stream_online {
                if let Err(error) = self.open_online() {
                    note_error(&format!(
                        "streaming model unavailable; using phrase chunks instead: {error:#}"
                    ));
                } else {
                    // The audio already in the recorder was not part of this mode.
                    self.online_fed = committed;
                    self.online_segment_start = committed;
                }
            }
            if let Ok(mut status) = self.status.lock() {
                status.streaming = true;
            }
            log::write("streaming adopted the current capture");
            return Ok(());
        }

        self.start_recording(true)
    }

    fn start_recording(&mut self, streaming: bool) -> Result<()> {
        let recorder = Recorder::start(self.config.device.as_deref())?;
        let device = recorder.device().to_string();
        // Bumping the generation is what cancels a decode that is still running.
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.unloaded = false;
        self.last_activity = Instant::now();
        self.started = Instant::now();
        self.recorder = Some(recorder);
        self.streaming = streaming;
        self.stream_committed = 0;
        self.stream_focus = if streaming { inject::focus() } else { None };
        self.stream_text.clear();
        self.set(Phase::Recording);
        if let Ok(mut status) = self.status.lock() {
            status.device = device.clone();
        }
        // Overlap the model load with the utterance: by the time the user stops
        // talking, the recognizer is warm.
        let _ = self.jobs.send(DecodeJob::Warm);
        if self.config.tones {
            tone::play(Cue::Start);
        }
        log::write(&format!(
            "recording on {device}{}",
            if streaming { " (streaming)" } else { "" }
        ));
        if streaming && self.config.stream_online {
            // The capture is already running, so a cold model load overlaps the
            // first words instead of delaying the toast or the microphone.
            if let Err(error) = self.open_online() {
                // The phrase-chunking path is still a usable streaming mode;
                // say why the word-by-word model is not in play.
                note_error(&format!(
                    "streaming model unavailable; using phrase chunks instead: {error:#}"
                ));
            }
        }
        Ok(())
    }

    fn stop_recording(&mut self) {
        self.recorder = None;
        self.online = None;
    }

    /// Load the streaming zipformer and open one stream for this session.
    fn open_online(&mut self) -> Result<()> {
        let dir = crate::model::streaming_dir()?;
        let sherpa = Sherpa::open()?;
        let recognizer = Arc::new(sherpa.online_recognizer(&dir, 4)?);
        self.online = Some(recognizer.stream()?);
        self.online_fed = 0;
        self.online_segment_start = 0;
        self.online_last.clear();
        self.online_pasted.clear();
        log::write(&format!("streaming model {}", dir.display()));
        Ok(())
    }

    fn stop_recording_and_decode(&mut self) -> Result<()> {
        let Some(recorder) = self.recorder.take() else {
            self.set(Phase::Idle);
            return Ok(());
        };
        // The window that should receive the paste, recorded before the decode
        // starts rather than after it.
        self.focus = inject::focus();

        // A device that died mid-capture is routine on Windows (unplug, Bluetooth
        // profile switch, sleep). Decode what arrived instead of throwing the
        // sentence away.
        let failure = recorder.failure();
        let (samples, rate) = recorder.finish_mono();
        let seconds = samples.len() as f32 / rate.max(1) as f32;
        if seconds < 0.3 {
            if let Some(reason) = failure {
                note(&format!(
                    "audio device failed before anything was captured: {reason}"
                ));
            }
            self.set(Phase::Idle);
            return Ok(());
        }
        if let Some(reason) = &failure {
            note(&format!(
                "audio device failed mid-capture ({reason}); delivering {seconds:.1}s"
            ));
        }
        if seconds > self.config.max_seconds {
            self.set(Phase::Idle);
            return Err(anyhow!("recording was longer than the configured maximum"));
        }

        let mut samples = audio::resample(&samples, rate, audio::SAMPLE_RATE)?;
        audio::normalize_loudness(&mut samples);

        self.set(Phase::Decoding);
        self.jobs
            .send(DecodeJob::Decode {
                samples,
                generation: self.generation.load(Ordering::SeqCst),
                chunk_seconds: self.config.chunk_seconds,
            })
            .map_err(|_| anyhow!("decoder thread is gone"))?;
        Ok(())
    }

    /// Commit a phrase while the user keeps talking.
    ///
    /// The capture is read as a snapshot, resampled, and sliced from the last
    /// committed sample. A commit waits for a pause near `stream_seconds` so a
    /// word is not cut in half; a long uninterrupted sentence forces one at
    /// 2.5x that length. Samples are marked committed before the decode runs,
    /// so a slow decode cannot send the same audio twice.
    fn stream_tick(&mut self) {
        if !self.streaming {
            return;
        }
        let phase = self.status.lock().map(|s| s.phase).unwrap_or(Phase::Idle);
        if phase != Phase::Recording {
            return;
        }
        if self.online.is_some() {
            if let Err(error) = self.online_tick() {
                note_error(&format!("streaming: {error:#}"));
                self.set(Phase::Idle);
            }
            return;
        }
        let Some((mono, rate)) = self
            .recorder
            .as_ref()
            .map(|recorder| recorder.snapshot_mono())
        else {
            return;
        };

        let elapsed = mono.len() as f32 / rate.max(1) as f32;
        let committed = self.stream_committed as f32 / rate.max(1) as f32;
        let target = self.config.stream_seconds.max(0.5);
        if elapsed - committed < target {
            return;
        }
        if self.stream_committed >= mono.len() {
            return;
        }

        let raw = &mono[self.stream_committed..];
        let Ok(mut chunk) = audio::resample(raw, rate, audio::SAMPLE_RATE) else {
            return;
        };
        if chunk.len() as f32 / (audio::SAMPLE_RATE as f32) < 0.25 {
            return;
        }
        let forced = elapsed - committed >= target * 2.5;
        if !forced && !silent_tail(raw, rate) {
            return;
        }

        audio::normalize_loudness(&mut chunk);
        self.stream_committed = mono.len();
        self.last_activity = Instant::now();
        if self
            .jobs
            .send(DecodeJob::Stream {
                samples: chunk,
                generation: self.generation.load(Ordering::SeqCst),
                final_chunk: false,
            })
            .is_err()
        {
            note_error("streaming: decoder thread is gone");
            self.set(Phase::Idle);
        }
    }

    /// Stop capturing and decode whatever has not been committed yet.
    fn stop_streaming(&mut self) -> Result<()> {
        if self.online.is_some() {
            return self.stop_online();
        }
        let Some(recorder) = self.recorder.take() else {
            self.set(Phase::Idle);
            return Ok(());
        };

        self.focus = self.stream_focus.clone();
        let failure = recorder.failure();
        let (mono, rate) = recorder.finish_mono();

        // A new generation revokes partial decodes that are still in flight;
        // only the final chunk is allowed to deliver after this point.
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        if let Some(reason) = failure {
            note(&format!("audio device failed mid-capture ({reason})"));
        }

        if self.stream_committed >= mono.len() {
            self.end_stream();
            return Ok(());
        }
        let raw = &mono[self.stream_committed..];
        if raw.len() as f32 / (rate.max(1) as f32) < 0.25 {
            self.end_stream();
            return Ok(());
        }
        let mut tail = match audio::resample(raw, rate, audio::SAMPLE_RATE) {
            Ok(tail) => tail,
            Err(error) => {
                self.set(Phase::Idle);
                return Err(error);
            }
        };
        audio::normalize_loudness(&mut tail);

        if let Ok(mut status) = self.status.lock() {
            status.phase = Phase::Decoding;
            status.seconds = 0.0;
            status.level = 0.0;
            status.generation = generation;
        }
        if self
            .jobs
            .send(DecodeJob::Stream {
                samples: tail,
                generation,
                final_chunk: true,
            })
            .is_err()
        {
            self.set(Phase::Idle);
            anyhow::bail!("decoder thread is gone");
        }
        Ok(())
    }

    /// Feed the online recognizer and paste newly confirmed words.
    fn online_tick(&mut self) -> Result<()> {
        let Some((mono, rate)) = self
            .recorder
            .as_ref()
            .map(|recorder| recorder.snapshot_mono())
        else {
            return Ok(());
        };
        if self.online_fed >= mono.len() {
            return Ok(());
        }
        let raw = &mono[self.online_fed..];
        // 50 ms of audio keeps the hypothesis moving without running the
        // feature frontend on every poll.
        if raw.len() < (rate as usize / 20).max(160) {
            return Ok(());
        }

        let (hypothesis, endpoint) = {
            let Some(stream) = self.online.as_ref() else {
                return Ok(());
            };
            // The streaming API takes the device rate and resamples internally,
            // so feeding it raw avoids padding a 16 kHz resampler at every
            // chunk edge.
            stream.accept(raw, rate)?;
            (stream.text()?, stream.endpoint())
        };
        let hypothesis = online_case(&hypothesis);
        self.online_fed = mono.len();

        let since_handoff =
            (self.online_fed - self.online_segment_start) as f32 / rate.max(1) as f32;
        let target = self.config.stream_segment_seconds.max(2.0);
        // Prefer to hand off at a pause, but never let one head grow past 1.5x
        // the target when a monologue has no pause at all.
        let handoff =
            since_handoff >= target && (silent_tail(&mono, rate) || since_handoff >= target * 1.5);

        if endpoint || handoff {
            if handoff && !endpoint {
                log::write(&format!(
                    "streaming: handed off after {since_handoff:.1}s without an endpoint"
                ));
            }
            self.commit_online_final(&hypothesis)?;
            if let Some(stream) = self.online.as_ref() {
                stream.reset();
            }
            self.online_last.clear();
            self.online_pasted.clear();
            self.online_segment_start = self.online_fed;
        } else {
            self.commit_online(&hypothesis)?;
            self.online_last = hypothesis;
        }
        Ok(())
    }

    /// Paste the longest common prefix with the previous hypothesis that ends on
    /// a word boundary. The tail is still being revised, so it waits.
    fn commit_online(&mut self, hypothesis: &str) -> Result<()> {
        let stable = stable_prefix(&self.online_last, hypothesis);
        if stable.len() <= self.online_pasted.len() {
            return Ok(());
        }
        if !stable.starts_with(&self.online_pasted) {
            // The model revised a word that is already in the document. It
            // cannot be taken back without editing the user's text, so keep the
            // head alive until the next endpoint or forced handoff starts a new
            // segment instead of stopping the whole session.
            log::write("streaming: revision past the pasted prefix; waiting for handoff");
            return Ok(());
        }
        let suffix = stable[self.online_pasted.len()..].trim();
        if suffix.is_empty() {
            return Ok(());
        }
        self.deliver_stream(suffix, false)?;
        self.online_pasted = stable;
        Ok(())
    }

    /// At an endpoint the hypothesis is final for this phrase, so paste all of
    /// it and let the recognizer start a new segment.
    fn commit_online_final(&mut self, hypothesis: &str) -> Result<()> {
        let text = hypothesis.trim();
        if text.is_empty() {
            return Ok(());
        }
        let pasted = self.online_pasted.trim();
        let suffix = if pasted.is_empty() {
            text
        } else if let Some(rest) = text.strip_prefix(pasted) {
            rest.trim()
        } else {
            log::write("streaming: final phrase revised pasted text; not re-pasting it");
            return Ok(());
        };
        if !suffix.is_empty() {
            self.deliver_stream(suffix, false)?;
        }
        self.online_pasted = text.to_string();
        Ok(())
    }

    /// Stop the online session: drain the last samples, paste the final phrase
    /// and release the model.
    fn stop_online(&mut self) -> Result<()> {
        let Some(recorder) = self.recorder.take() else {
            self.end_stream();
            return Ok(());
        };
        let (mono, rate) = recorder.finish_mono();
        if self.online_fed < mono.len() {
            if let Some(stream) = self.online.as_ref() {
                let _ = stream.accept(&mono[self.online_fed..], rate);
            }
        }

        let text = match self.online.as_ref() {
            Some(stream) => online_case(stream.finish()?.trim()),
            None => String::new(),
        };
        if !text.is_empty() {
            let pasted = self.online_pasted.trim();
            let suffix = if pasted.is_empty() {
                Some(text.as_str())
            } else {
                text.strip_prefix(pasted).map(str::trim)
            };
            match suffix {
                Some(suffix) if !suffix.is_empty() => {
                    self.deliver_stream(suffix, true)?;
                }
                Some(_) => {}
                None => {
                    inject::notify(
                        "Streaming stopped",
                        "The final words were revised and not pasted",
                    );
                }
            }
        }

        self.online = None;
        self.online_fed = 0;
        self.online_last.clear();
        self.online_pasted.clear();
        self.end_stream();
        Ok(())
    }

    fn streamed(&mut self, generation: u64, text: String, final_chunk: bool) -> Result<()> {
        if !self.streaming || generation != self.generation.load(Ordering::SeqCst) {
            return Ok(());
        }
        let text = text.trim();
        if text.is_empty() {
            if final_chunk {
                self.end_stream();
            }
            return Ok(());
        }

        self.deliver_stream(text, final_chunk)?;
        if final_chunk {
            self.end_stream();
        }
        Ok(())
    }

    /// Copy and paste one piece of a streaming session.
    ///
    /// Both the phrase-chunked fallback and the online recognizer go through
    /// this, so the focus guard, clipboard behaviour and history are identical.
    fn deliver_stream(&mut self, text: &str, final_chunk: bool) -> Result<()> {
        let now = inject::focus();
        if !same_focus(self.stream_focus.as_ref(), now.as_ref()) {
            inject::notify(
                "Streaming stopped",
                "Focus changed; the rest was not pasted",
            );
            return Err(anyhow!("focus changed; streaming stopped"));
        }

        // Every chunk gets a separating space except the last one. The user
        // chose `trailing_space` for single dictations; chunk boundaries need a
        // space regardless or words run together.
        let payload = if final_chunk {
            text.to_string()
        } else {
            format!("{text} ")
        };

        self.stream_text.push_str(text);
        self.stream_text.push(' ');

        if self.fake_delivery {
            log::write("streamed (fake delivery)");
            return Ok(());
        }

        inject::copy(&payload)?;
        match inject::paste(self.config.paste, now.as_ref()) {
            Ok(crate::config::PasteMethod::None) => {
                inject::notify("Streaming stopped", "No paste tool available");
                return Err(anyhow!("no paste tool"));
            }
            Ok(_) => {}
            Err(error) => return Err(error).context("stream paste"),
        }
        self.last_activity = Instant::now();
        if let Ok(mut status) = self.status.lock() {
            status.last_text = Some(text.to_string());
        }
        log::write(&format!("streamed {} chars", text.chars().count()));
        Ok(())
    }

    /// Flush the session's history, play the stop cue and leave Recording.
    fn end_stream(&mut self) {
        self.finish_stream_history();
        if self.config.tones {
            tone::play(Cue::Done);
        }
        self.set(Phase::Idle);
    }

    /// Store the whole streamed session, not one row per phrase.
    fn finish_stream_history(&mut self) {
        if !self.config.history {
            return;
        }
        let text = self.stream_text.trim();
        if text.is_empty() {
            return;
        }
        if let Err(error) = crate::history::append(text) {
            note(&format!("history: {error:#}"));
        }
        let _ = crate::history::prune(self.config.history_days);
        self.stream_text.clear();
    }

    fn delivered(&mut self, generation: u64, text: String) -> Result<()> {
        // A late result from a cancelled capture must not be delivered into the
        // next one.
        if generation != self.generation.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.last_activity = Instant::now();
        let text = text.trim().to_string();
        if text.is_empty() {
            self.set(Phase::Idle);
            return Ok(());
        }

        let text = if self.config.trailing_space {
            format!("{text} ")
        } else {
            text
        };

        if let Ok(mut status) = self.status.lock() {
            status.last_text = Some(text.trim_end().to_string());
        }

        if self.fake_delivery {
            log::write("delivered (fake delivery)");
            self.set(Phase::Idle);
            return Ok(());
        }

        inject::copy(&text)?;
        if self.config.history {
            if let Err(error) = crate::history::append(text.trim_end()) {
                note(&format!("history: {error:#}"));
            }
            let _ = crate::history::prune(self.config.history_days);
        }

        let focus_now = inject::focus();
        if !same_focus(self.focus.as_ref(), focus_now.as_ref()) {
            inject::notify("Transcript copied", "Focus changed; paste it yourself");
            self.set(Phase::Idle);
            return Ok(());
        }

        let method = inject::paste(self.config.paste, focus_now.as_ref());
        match method {
            Ok(crate::config::PasteMethod::None) => {
                inject::notify("Transcript copied", "No paste tool available");
            }
            Ok(_) => {}
            Err(error) => {
                note_error(&format!("paste failed: {error:#}"));
                inject::notify("Transcript copied", "Paste failed; paste it yourself");
            }
        }
        if self.config.tones {
            tone::play(Cue::Done);
        }
        log::write(&format!(
            "delivered {} chars",
            text.trim_end().chars().count()
        ));
        self.set(Phase::Idle);
        Ok(())
    }

    /// Stop by itself once a capture passes the configured maximum, so a stuck
    /// key or a forgotten session cannot record for ever.
    fn enforce_max_capture(&mut self) {
        let limit = self.config.max_seconds;
        if limit <= 0.0 {
            return;
        }
        let recording = self
            .status
            .lock()
            .map(|status| status.phase)
            .unwrap_or(Phase::Idle)
            == Phase::Recording;
        if recording && self.started.elapsed().as_secs_f32() > limit {
            note(&format!("capture reached the {limit:.0}s limit; stopping"));
            let outcome = if self.streaming {
                self.stop_streaming()
            } else {
                self.stop_recording_and_decode()
            };
            if let Err(error) = outcome {
                note_error(&format!("stop at the limit failed: {error:#}"));
                self.set(Phase::Idle);
            }
        }
    }

    /// Drop the warm model after a long idle. The next capture reloads it while
    /// the user is already speaking, so the cost is hidden.
    fn unload_if_idle(&mut self) {
        let minutes = self.config.idle_unload_minutes;
        if minutes == 0 || self.unloaded {
            return;
        }
        let idle = self.last_activity.elapsed() >= Duration::from_secs(minutes as u64 * 60);
        let phase = self.status.lock().map(|s| s.phase).unwrap_or(Phase::Idle);
        if idle && phase == Phase::Idle {
            if self.jobs.send(DecodeJob::Unload).is_ok() {
                self.unloaded = true;
            }
        }
    }

    fn set(&mut self, phase: Phase) {
        if phase == Phase::Idle {
            self.streaming = false;
            self.online = None;
            self.online_fed = 0;
            self.online_segment_start = 0;
            self.online_last.clear();
            self.online_pasted.clear();
        }
        if let Ok(mut status) = self.status.lock() {
            status.phase = phase;
            status.generation = self.generation.load(Ordering::SeqCst);
            status.seconds = if phase == Phase::Recording {
                self.started.elapsed().as_secs_f32()
            } else {
                0.0
            };
            status.level = 0.0;
            status.bands.clear();
            status.streaming = self.streaming;
        }
    }

    fn refresh_seconds(&self) {
        if let Ok(mut status) = self.status.lock() {
            if status.phase == Phase::Recording
                && status.generation == self.generation.load(Ordering::SeqCst)
            {
                status.seconds = self.started.elapsed().as_secs_f32();
                status.level = self
                    .recorder
                    .as_ref()
                    .map(|recorder| recorder.level())
                    .unwrap_or(0.0);
                #[cfg(windows)]
                {
                    status.bands = self
                        .recorder
                        .as_ref()
                        .map(|recorder| recorder.bands(SPECTRUM_BANDS))
                        .unwrap_or_default();
                }
            }
        }
    }
}

/// Whether the transcript may be pasted where the focus is now.
///
/// A window that changed during the decode means the user moved on, and pasting
/// there is worse than not pasting. If focus was never readable there is nothing
/// to compare, so the paste proceeds; if it was readable at stop but is not now,
/// it does not.
fn same_focus(captured: Option<&Focus>, now: Option<&Focus>) -> bool {
    match captured {
        None => true,
        Some(captured) => now.is_some_and(|now| captured == now),
    }
}

/// The longest prefix of two online hypotheses that ends on a word boundary.
///
/// A hypothesis that has not changed still only confirms the words before its
/// last space: the model may be about to revise the trailing word, and once
/// text is pasted it cannot be taken back without editing the user's document.
fn stable_prefix(previous: &str, current: &str) -> String {
    let mut common = String::new();
    for (before, now) in previous.chars().zip(current.chars()) {
        if before != now {
            break;
        }
        common.push(before);
    }
    match common.rfind(' ') {
        Some(index) => common[..index].trim_end().to_string(),
        None => String::new(),
    }
}

/// The streaming zipformer is trained on upper-case LibriSpeech transcripts.
///
/// Lower-case the hypothesis and capitalise the first word of the segment, plus
/// the pronoun "I" and its contractions. Proper nouns cannot be recovered without
/// a true-casing model, and no punctuation is invented: inserting a wrong period
/// is worse than leaving the phrase open.
fn online_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut capitalize = true;
    for character in text.chars() {
        if character.is_alphabetic() {
            if capitalize {
                out.extend(character.to_uppercase());
                capitalize = false;
            } else {
                out.extend(character.to_lowercase());
            }
        } else {
            out.push(character);
            if matches!(character, '.' | '!' | '?' | '\n') {
                capitalize = true;
            }
        }
    }
    out.split(' ')
        .map(|token| {
            if token == "i" {
                "I".to_string()
            } else if token.starts_with("i'") {
                format!("I{}", &token[1..])
            } else {
                token.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether the end of a capture is quiet enough to cut a phrase there.
///
/// The threshold is on raw device levels: speech sits well above it, room noise
/// below. A false negative only delays the commit, because `stream_tick` forces
/// one after a while.
fn silent_tail(samples: &[f32], rate: u32) -> bool {
    let window = (rate as f32 * 0.25) as usize;
    if samples.len() < window {
        return false;
    }
    let tail = &samples[samples.len() - window..];
    let energy: f32 = tail.iter().map(|sample| sample * sample).sum::<f32>() / window as f32;
    energy.sqrt() < 0.008
}

/// Load the configuration and the model location, then run. This is what both
/// `hippo-voice daemon` and the `hippo-voice-daemon` binary call.
pub fn run_process() -> Result<()> {
    crate::log::write(&format!(
        "daemon: starting {} ({})",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    ));
    let config = Config::load()?;
    // With a fake recognizer there is no model to find, which is how the daemon
    // is tested on a machine that has never downloaded one.
    let model = if std::env::var_os("HIPPO_VOICE_FAKE_MODEL").is_some() {
        PathBuf::new()
    } else {
        crate::model::resolve_dir()?
    };
    run(config, model, 4)
}

/// Run the daemon in the foreground until it is killed.
pub fn run(config: Config, model_dir: PathBuf, threads: i32) -> Result<()> {
    if let Some(dir) = transport::endpoint_path()?.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = Listener::bind()?;

    let status = Arc::new(Mutex::new(Status::default()));
    let generation = Arc::new(AtomicU64::new(0));
    let (events_tx, events_rx) = mpsc::channel();
    let (jobs_tx, jobs_rx) = mpsc::channel();

    if let Ok(mut view) = status.lock() {
        view.hotkey = Some(if cfg!(windows) {
            "registering".to_string()
        } else {
            "compositor binding".to_string()
        });
    }

    #[cfg(windows)]
    crate::tray::spawn(events_tx.clone())?;

    #[cfg(windows)]
    crate::overlay::spawn(Arc::clone(&status))?;

    #[cfg(windows)]
    {
        let chord = crate::chord::Chord::parse(&config.hotkey).unwrap_or_else(|error| {
            note(&format!("hotkey: {error}; falling back to ctrl+space"));
            crate::chord::Chord::parse("ctrl+space").expect("the default chord is valid")
        });
        let stream = crate::chord::Chord::parse(&config.stream_hotkey).unwrap_or_else(|error| {
            note(&format!(
                "stream hotkey: {error}; falling back to ctrl+shift+space"
            ));
            crate::chord::Chord::parse("ctrl+shift+space")
                .expect("the default streaming chord is valid")
        });
        crate::hotkey::spawn(events_tx.clone(), chord, stream, Arc::clone(&status))?;
    }

    let decoder_events = events_tx.clone();
    let decoder_generation = Arc::clone(&generation);
    let fake = fake_model();
    thread::Builder::new()
        .name("decoder".into())
        .spawn(move || {
            decoder_loop(
                jobs_rx,
                decoder_events,
                model_dir,
                threads,
                decoder_generation,
                fake,
            )
        })?;

    install_listener(listener, Arc::clone(&status), events_tx.clone())?;

    let mut daemon = Daemon {
        config,
        jobs: jobs_tx,
        status,
        recorder: None,
        generation: Arc::clone(&generation),
        unloaded: true,
        last_activity: Instant::now(),
        started: Instant::now(),
        focus: None,
        fake_delivery: fake_delivery(),
        streaming: false,
        stream_committed: 0,
        stream_focus: None,
        stream_text: String::new(),
        online: None,
        online_fed: 0,
        online_segment_start: 0,
        online_last: String::new(),
        online_pasted: String::new(),
    };

    loop {
        match events_rx.recv_timeout(TICK) {
            Ok(Event::Quit) => break,
            Ok(Event::Toggle) => {
                if let Err(error) = daemon.handle(Event::Toggle) {
                    note_error(&format!("toggle: {error:#}"));
                }
            }
            Ok(event) => {
                if let Err(error) = daemon.handle(event) {
                    note_error(&format!("event: {error:#}"));
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                daemon.refresh_seconds();
                daemon.stream_tick();
                daemon.unload_if_idle();
                daemon.enforce_max_capture();
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn install_listener(
    listener: Listener,
    status: Arc<Mutex<Status>>,
    events: Sender<Event>,
) -> Result<()> {
    let token = listener.token();
    thread::Builder::new()
        .name("listener".into())
        .spawn(move || {
            while let Ok(stream) = listener.accept() {
                let status = Arc::clone(&status);
                let events = events.clone();
                let token = token.clone();
                thread::spawn(move || {
                    let _ = serve(stream, status, events, token);
                });
            }
        })?;
    Ok(())
}

fn serve(
    mut stream: Box<dyn Stream>,
    status: Arc<Mutex<Status>>,
    events: Sender<Event>,
    token: Option<String>,
) -> Result<()> {
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line)?;
    let request: Request =
        serde_json::from_str(line.trim()).context(if line.trim().is_empty() {
            "empty request".to_string()
        } else {
            format!("parse request {line:?}")
        })?;

    // Windows only: the ephemeral port is not authority, the token is.
    if let Some(expected) = token {
        if request.token.as_deref() != Some(expected.as_str()) {
            anyhow::bail!("request did not carry the daemon's token");
        }
    }

    let reply = match request.cmd.as_str() {
        "toggle" => {
            let _ = events.send(Event::Toggle);
            Reply {
                ok: true,
                app: Some(APP.to_string()),
                protocol: Some(PROTOCOL),
                phase: None,
                seconds: None,
                device: None,
                text: None,
                hotkey: None,
                error: None,
            }
        }
        "quit" => {
            let _ = events.send(Event::Quit);
            Reply {
                ok: true,
                app: Some(APP.to_string()),
                protocol: Some(PROTOCOL),
                phase: None,
                seconds: None,
                device: None,
                text: None,
                hotkey: None,
                error: None,
            }
        }
        "status" => {
            let snapshot = status.lock().map(|s| s.clone()).unwrap_or_default();
            Reply {
                ok: true,
                app: Some(APP.to_string()),
                protocol: Some(PROTOCOL),
                phase: Some(snapshot.phase.arg().to_string()),
                seconds: Some(snapshot.seconds),
                device: Some(snapshot.device),
                text: snapshot.last_text,
                hotkey: snapshot.hotkey,
                error: None,
            }
        }
        other => Reply {
            ok: false,
            app: Some(APP.to_string()),
            protocol: Some(PROTOCOL),
            phase: None,
            seconds: None,
            device: None,
            text: None,
            hotkey: None,
            error: Some(format!("unknown command {other:?}")),
        },
    };

    serde_json::to_writer(&mut stream, &reply)?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn decoder_loop(
    jobs: Receiver<DecodeJob>,
    events: Sender<Event>,
    model_dir: PathBuf,
    threads: i32,
    generation: Arc<AtomicU64>,
    fake: Option<String>,
) {
    let mut loaded: Option<crate::sherpa::Recognizer> = None;

    while let Ok(job) = jobs.recv() {
        match job {
            DecodeJob::Warm => {
                if fake.is_some() {
                    continue;
                }
                if loaded.is_none() {
                    match load(&model_dir, threads) {
                        Ok(pair) => loaded = Some(pair),
                        Err(error) => {
                            let _ = events.send(Event::Failed(format!("{error:#}")));
                        }
                    }
                }
            }
            DecodeJob::Stream {
                samples,
                generation: job_generation,
                final_chunk,
            } => {
                if let Some(canned) = &fake {
                    thread::sleep(Duration::from_millis(20));
                    let _ = events.send(Event::Streamed {
                        generation: job_generation,
                        text: canned.clone(),
                        final_chunk,
                    });
                    continue;
                }
                if generation.load(Ordering::SeqCst) != job_generation {
                    continue;
                }
                if loaded.is_none() {
                    match load(&model_dir, threads) {
                        Ok(pair) => loaded = Some(pair),
                        Err(error) => {
                            let _ = events.send(Event::Failed(format!("{error:#}")));
                            continue;
                        }
                    }
                }
                let Some(recognizer) = loaded.as_ref() else {
                    continue;
                };
                match recognizer.transcribe(&samples) {
                    Ok(text) => {
                        if generation.load(Ordering::SeqCst) == job_generation {
                            let _ = events.send(Event::Streamed {
                                generation: job_generation,
                                text,
                                final_chunk,
                            });
                        }
                    }
                    Err(error) => {
                        let _ = events.send(Event::Failed(format!("{error:#}")));
                    }
                }
            }
            DecodeJob::Unload => {
                loaded = None;
            }
            DecodeJob::Decode {
                samples,
                generation: job_generation,
                chunk_seconds,
            } => {
                if let Some(canned) = &fake {
                    // Stand in for a real decode, including its latency shape.
                    // Checked first: a fake run has no model on disk to load.
                    thread::sleep(Duration::from_millis(20));
                    let _ = events.send(Event::Decoded {
                        generation: job_generation,
                        text: canned.clone(),
                    });
                    continue;
                }
                if loaded.is_none() {
                    match load(&model_dir, threads) {
                        Ok(pair) => loaded = Some(pair),
                        Err(error) => {
                            let _ = events.send(Event::Failed(format!("{error:#}")));
                            continue;
                        }
                    }
                }

                let Some(recognizer) = loaded.as_ref() else {
                    continue;
                };

                // Cut at silences, and stop between chunks if the user pressed
                // the key again. A single decode call is not interruptible, so
                // the chunk boundary is the cancel granularity.
                let mut parts = Vec::new();
                let mut abandoned = false;
                for (start, end) in crate::audio::split_at_silence(&samples, chunk_seconds) {
                    if generation.load(Ordering::SeqCst) != job_generation {
                        abandoned = true;
                        break;
                    }
                    match recognizer.transcribe(&samples[start..end]) {
                        Ok(text) if !text.is_empty() => parts.push(text),
                        Ok(_) => {}
                        Err(error) => {
                            let _ = events.send(Event::Failed(format!("{error:#}")));
                            abandoned = true;
                            break;
                        }
                    }
                }
                if !abandoned {
                    let _ = events.send(Event::Decoded {
                        generation: job_generation,
                        text: parts.join(" "),
                    });
                }
            }
        }
    }
}

/// Load the runtime and recognizer, then forget the borrow so the pair can live
/// in a long-lived variable.
fn load(model_dir: &Path, threads: i32) -> Result<crate::sherpa::Recognizer> {
    let sherpa = Sherpa::open()?;
    sherpa.recognizer(model_dir, threads)
}

#[cfg(test)]
mod case_tests {
    use super::*;

    #[test]
    fn the_streaming_model_is_sentence_cased() {
        assert_eq!(
            online_case("AFTER EARLY NIGHTFALL I SAID"),
            "After early nightfall I said"
        );
        assert_eq!(online_case("I'M HERE. NEXT ONE"), "I'm here. Next one");
    }
}

#[cfg(all(test, unix))]
mod focus_tests {
    use super::*;

    fn at(app: &str) -> Focus {
        Focus {
            app_id: app.to_string(),
            title: "window".to_string(),
        }
    }

    #[test]
    fn an_unchanged_window_is_pasted_into() {
        assert!(same_focus(Some(&at("editor")), Some(&at("editor"))));
    }

    #[test]
    fn a_window_that_changed_during_the_decode_is_not() {
        assert!(!same_focus(Some(&at("editor")), Some(&at("browser"))));
    }

    #[test]
    fn focus_we_could_never_read_still_pastes() {
        // No compositor API: there is nothing to compare, and refusing would
        // make the app useless on that desktop.
        assert!(same_focus(None, None));
    }

    #[test]
    fn losing_focus_after_capturing_it_does_not_paste() {
        assert!(!same_focus(Some(&at("editor")), None));
    }
}
