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
use crate::config::{Config, Stamp};
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
    /// How a transcript is delivered, as configured. Mirrors `config.toml` so
    /// `hippo-voice status` can answer "which paste tool is it actually using".
    #[serde(default)]
    pub paste: String,
    /// Bumped every time an edited `config.toml` is applied, so a client can tell
    /// that a save has landed. Hotkey changes still need a restart.
    #[serde(default)]
    pub config_revision: u64,
    /// Why the last re-read of `config.toml` failed, if it did. The last good
    /// config stays in force.
    #[serde(default)]
    pub config_error: Option<String>,
    /// Why the last dictation failed, if it did: a model that will not load, a
    /// decoder that panicked, a clipboard tool that is not installed. Cleared
    /// when a transcript is delivered, so it always describes the last cycle.
    #[serde(default)]
    pub last_error: Option<String>,
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
            paste: String::new(),
            config_revision: 0,
            config_error: None,
            last_error: None,
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
    pub paste: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
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
    /// Where the config came from and whether the file has changed since it was
    /// read. `None` when there was no file at the last look.
    config_path: PathBuf,
    config_stamp: Option<Stamp>,
    /// A config read while a capture was in flight. Swapping the device or the
    /// tones under a recording is not something the user asked for, so it waits
    /// for the next capture.
    pending_config: Option<Config>,
    config_revision: u64,
    config_error: Option<String>,
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
    /// Handle one event. Any failure is recorded, reported and cleared up here so
    /// the run loop only has to keep going.
    fn handle(&mut self, event: Event) -> Result<()> {
        match self.dispatch(event) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.fail(&format!("{error:#}"));
                Err(error)
            }
        }
    }

    fn dispatch(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Toggle => self.toggle(),
            Event::ToggleStream => self.toggle_stream(),
            // Handled by the run loop, which breaks out of it.
            Event::Quit => Ok(()),
            Event::Decoded { generation, text } => self.delivered(generation, text),
            Event::Streamed {
                generation,
                text,
                final_chunk,
            } => self.streamed(generation, text, final_chunk),
            Event::Failed(error) => {
                self.fail(&error);
                Ok(())
            }
        }
    }

    /// Record why a dictation did not happen, and say so.
    ///
    /// The tone only says "started" and "done"; without this the signal for a
    /// model that will not load, or a paste tool that is no longer installed, is
    /// a transcript that never arrives. The reason goes into `status`, which a
    /// client or the overlay can read, and to a desktop notification for the user
    /// who is looking at the window they were typing in.
    fn fail(&mut self, reason: &str) {
        note_error(&format!("failed: {reason}"));
        self.stop_recording();
        self.set(Phase::Idle);
        if let Ok(mut status) = self.status.lock() {
            status.last_error = Some(reason.to_string());
        }
        // A test run must not pop a notification on the machine running it.
        if !self.fake_delivery {
            inject::notify("Hippo Voice did not transcribe", reason);
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
        // An edit that arrived mid-capture lands here, before anything reads the
        // config for the new recording.
        if let Some(config) = self.pending_config.take() {
            self.apply_config(config);
        }
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
            self.clear_error();
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
        self.clear_error();
        self.set(Phase::Idle);
        Ok(())
    }

    /// A transcript that landed means the last failure is no longer the last word.
    fn clear_error(&mut self) {
        if let Ok(mut status) = self.status.lock() {
            status.last_error = None;
        }
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

    /// Re-read `config.toml` when the file has changed on disk.
    ///
    /// The settings window writes the file and exits, so this is what makes that
    /// write mean anything without a restart. A parse failure keeps the last good
    /// config in force and reports why in `status`: a typo in a hand-edit must
    /// not silently reset every setting the user has.
    fn reload_config(&mut self) {
        let stamp = Stamp::of(&self.config_path);
        if stamp == self.config_stamp {
            return;
        }
        // Remember the stamp before parsing, so a broken file is reported once
        // rather than on every tick.
        self.config_stamp = stamp;
        match Config::load_from(&self.config_path) {
            Ok(config) => {
                let phase = self.status.lock().map(|s| s.phase).unwrap_or(Phase::Idle);
                if phase == Phase::Idle {
                    self.apply_config(config);
                } else {
                    self.pending_config = Some(config);
                    log::write("config read; waiting for the capture to finish");
                }
            }
            Err(error) => {
                // The status field is already labelled `config:`, so it carries
                // the reason alone; the log keeps the sentence that explains it.
                let message = format!("{error:#}");
                note_error(&format!("config not applied: {message}"));
                self.config_error = Some(message.clone());
                if let Ok(mut status) = self.status.lock() {
                    status.config_error = Some(message);
                }
            }
        }
    }

    /// Put a config in force and record that it landed.
    fn apply_config(&mut self, config: Config) {
        let previous = std::mem::replace(&mut self.config, config);
        self.config_revision += 1;
        self.config_error = None;
        let revision = self.config_revision;
        if let Ok(mut status) = self.status.lock() {
            status.config_revision = revision;
            status.config_error = None;
            status.paste = self.config.paste.to_string();
        }
        log::write(&format!("config applied (revision {revision})"));
        // The Windows hotkey thread holds the chords it registered at startup.
        // Nothing re-registers them, so say so instead of pretending.
        if previous.hotkey != self.config.hotkey
            || previous.stream_hotkey != self.config.stream_hotkey
        {
            note("a hotkey change needs a daemon restart");
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
        if idle && phase == Phase::Idle && self.jobs.send(DecodeJob::Unload).is_ok() {
            self.unloaded = true;
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

/// Find the model location, then run. This is what both `hippo-voice daemon` and
/// the `hippo-voice-daemon` binary call; the configuration is read inside `run`,
/// which also re-reads it when the file changes.
pub fn run_process() -> Result<()> {
    // Before anything that can panic: a crash has to leave a line behind.
    crate::log::install_panic_hook();
    crate::log::write(&format!(
        "daemon: starting {} ({})",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    ));
    // With a fake recognizer there is no model to find, which is how the daemon
    // is tested on a machine that has never downloaded one.
    let model = if std::env::var_os("HIPPO_VOICE_FAKE_MODEL").is_some() {
        PathBuf::new()
    } else {
        crate::model::resolve_dir()?
    };
    run(model, 4)
}

/// Run the daemon in the foreground until it is killed.
///
/// The config is resolved and read here rather than passed in: the tick re-reads
/// it whenever the file changes, so this is the one place that decides where it
/// lives, and a file that will not parse must leave the daemon running on the
/// defaults instead of refusing to start.
pub fn run(model_dir: PathBuf, threads: i32) -> Result<()> {
    let config_path = Config::path()?;
    let (config, config_error) = match Config::load_from(&config_path) {
        Ok(config) => (config, None),
        Err(error) => {
            let message = format!("config not read; using the defaults: {error:#}");
            note_error(&message);
            (Config::default(), Some(message))
        }
    };
    let config_stamp = Stamp::of(&config_path);
    // `resolve_dir` returns an installed path whether or not the files are there,
    // so a fresh install would otherwise fail at the first utterance with no
    // explanation. An empty path means a fake recognizer is in play.
    let model_problem = if model_dir.as_os_str().is_empty() {
        None
    } else {
        crate::model::unusable_reason(&model_dir)
    };
    if let Some(problem) = &model_problem {
        note_error(&format!("model: {problem}"));
    }
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
        view.paste = config.paste.to_string();
        view.config_error = config_error.clone();
        view.last_error = model_problem.clone();
    }

    if let Some(problem) = &model_problem {
        // The one failure worth interrupting for: without a model nothing works,
        // and the fix is a command the user has to be told.
        if !fake_delivery() {
            inject::notify("Hippo Voice has no model", problem);
        }
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
        config_path,
        config_stamp,
        pending_config: None,
        config_revision: 0,
        config_error,
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
            // `handle` records any failure in `status`, notifies and resets the
            // phase itself, so the loop only has to keep going.
            Ok(event) => {
                let _ = daemon.handle(event);
            }
            Err(RecvTimeoutError::Timeout) => {
                daemon.reload_config();
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
                paste: None,
                config_revision: None,
                config_error: None,
                last_error: None,
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
                paste: None,
                config_revision: None,
                config_error: None,
                last_error: None,
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
                paste: Some(snapshot.paste),
                config_revision: Some(snapshot.config_revision),
                config_error: snapshot.config_error,
                last_error: snapshot.last_error,
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
            paste: None,
            config_revision: None,
            config_error: None,
            last_error: None,
            error: Some(format!("unknown command {other:?}")),
        },
    };

    serde_json::to_writer(&mut stream, &reply)?;
    stream.write_all(b"\n")?;
    Ok(())
}

/// How long `HIPPO_VOICE_FAKE_DECODE=slow` spends per job. Long enough that a
/// second press lands while the decoder is still working, which is the case the
/// phase machine has to survive.
const SLOW_DECODE: Duration = Duration::from_millis(400);

/// State for the decoder thread: one warm recognizer, plus the two test switches.
///
/// A `struct` rather than a pile of locals because the panic boundary has to cover
/// the work but not the channel: the loop keeps running, the recognizer is thrown
/// away, and `Event::Failed` tells the user what happened.
struct Decoder<'a> {
    loaded: Option<crate::sherpa::Recognizer>,
    model_dir: &'a Path,
    threads: i32,
    generation: &'a AtomicU64,
    /// A canned transcript, when the tests asked for one.
    fake: Option<&'a str>,
    /// `HIPPO_VOICE_FAKE_DECODE`: `panic` fails the first decode, `slow` delays
    /// every one.
    fake_decode: Option<&'a str>,
    /// Set before the fake panic fires, so only the first decode fails and the
    /// recovery is testable.
    panicked: bool,
}

impl Decoder<'_> {
    /// Decode one job. `None` when it produced nothing worth reporting.
    fn run(&mut self, job: DecodeJob) -> Option<Event> {
        if matches!(&job, DecodeJob::Decode { .. } | DecodeJob::Stream { .. }) {
            self.misbehave();
        }
        match job {
            DecodeJob::Warm => {
                if self.fake.is_some() {
                    return None;
                }
                // Warm is the load overlapped with the first words; a failure
                // here is reported once and the decode reports it again rather
                // than loading twice.
                self.ensure_loaded().err()
            }
            DecodeJob::Stream {
                samples,
                generation,
                final_chunk,
            } => {
                if let Some(canned) = self.fake {
                    thread::sleep(Duration::from_millis(20));
                    return Some(Event::Streamed {
                        generation,
                        text: canned.to_string(),
                        final_chunk,
                    });
                }
                if self.generation.load(Ordering::SeqCst) != generation {
                    return None;
                }
                if let Err(failure) = self.ensure_loaded() {
                    return Some(failure);
                }
                let recognizer = self.loaded.as_ref()?;
                match recognizer.transcribe(&samples) {
                    Ok(text) if self.generation.load(Ordering::SeqCst) == generation => {
                        Some(Event::Streamed {
                            generation,
                            text,
                            final_chunk,
                        })
                    }
                    Ok(_) => None,
                    Err(error) => Some(Event::Failed(format!("{error:#}"))),
                }
            }
            DecodeJob::Unload => {
                self.loaded = None;
                None
            }
            DecodeJob::Decode {
                samples,
                generation,
                chunk_seconds,
            } => {
                if let Some(canned) = self.fake {
                    // Stand in for a real decode, including its latency shape.
                    // Checked first: a fake run has no model on disk to load.
                    thread::sleep(Duration::from_millis(20));
                    return Some(Event::Decoded {
                        generation,
                        text: canned.to_string(),
                    });
                }
                if let Err(failure) = self.ensure_loaded() {
                    return Some(failure);
                }
                let recognizer = self.loaded.as_ref()?;

                // Cut at silences, and stop between chunks if the user pressed
                // the key again. A single decode call is not interruptible, so
                // the chunk boundary is the cancel granularity.
                let mut parts = Vec::new();
                for (start, end) in crate::audio::split_at_silence(&samples, chunk_seconds) {
                    if self.generation.load(Ordering::SeqCst) != generation {
                        return None;
                    }
                    match recognizer.transcribe(&samples[start..end]) {
                        Ok(text) if !text.is_empty() => parts.push(text),
                        Ok(_) => {}
                        Err(error) => return Some(Event::Failed(format!("{error:#}"))),
                    }
                }
                Some(Event::Decoded {
                    generation,
                    text: parts.join(" "),
                })
            }
        }
    }

    /// Load on demand, returning the event to report when the load fails.
    fn ensure_loaded(&mut self) -> std::result::Result<(), Event> {
        if self.loaded.is_some() {
            return Ok(());
        }
        match load(self.model_dir, self.threads) {
            Ok(recognizer) => {
                self.loaded = Some(recognizer);
                Ok(())
            }
            Err(error) => Err(Event::Failed(format!("{error:#}"))),
        }
    }

    /// The test-only misbehaviour switch. A real decoder has no equivalent, which
    /// is why it lives behind an environment variable rather than a config key.
    fn misbehave(&mut self) {
        match self.fake_decode {
            Some("panic") if !self.panicked => {
                self.panicked = true;
                panic!("fake decoder panic");
            }
            Some("slow") => thread::sleep(SLOW_DECODE),
            _ => {}
        }
    }
}

/// What a panic payload says, since Rust keeps it as an `Any`.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "no message".to_string()
    }
}

fn decoder_loop(
    jobs: Receiver<DecodeJob>,
    events: Sender<Event>,
    model_dir: PathBuf,
    threads: i32,
    generation: Arc<AtomicU64>,
    fake: Option<String>,
) {
    let fake_decode = std::env::var("HIPPO_VOICE_FAKE_DECODE").ok();
    let mut decoder = Decoder {
        loaded: None,
        model_dir: &model_dir,
        threads,
        generation: &generation,
        fake: fake.as_deref(),
        fake_decode: fake_decode.as_deref(),
        panicked: false,
    };

    while let Ok(job) = jobs.recv() {
        // A panic in a decode must cost one dictation, not the session: the
        // receiver lives out here, so the thread survives and the next press
        // still works.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decoder.run(job)));
        match outcome {
            Ok(Some(event)) => {
                let _ = events.send(event);
            }
            Ok(None) => {}
            Err(payload) => {
                // The recognizer was inside a native call when it unwound, so its
                // state is unknown. Drop it: the next capture loads a fresh one.
                decoder.loaded = None;
                let _ = events.send(Event::Failed(format!(
                    "the decoder panicked: {}",
                    panic_message(&*payload)
                )));
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
