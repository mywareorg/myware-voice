//! The resident process: one microphone, one warm recognizer, one socket.
//!
//! `myware-voice toggle` — what the compositor keybind runs — is a client of this
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

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::audio;
use crate::capture::Recorder;
use crate::config::{Clipboard, Config, Stamp};
use crate::inject::{self, Focus};
use crate::licence::{self, Tier};
use crate::log;
use crate::sherpa::Sherpa;
use crate::tone::{self, Cue};
use crate::transport::{self, Listener, Stream};

/// Longest pause between event-loop polls. Also the status freshness.
const TICK: Duration = Duration::from_millis(100);

/// How long the transcript stays on the clipboard after the paste chord.
///
/// The chord only queues the paste: the target application reads the selection
/// afterwards, and a `wl-copy` that replaces it in between makes that read fail or
/// hand over the wrong bytes. A delay long enough to cover the read is the price
/// of not needing a clipboard-manager protocol; the tick rounds it up to the next
/// poll, so the real wait is this plus up to one `TICK`.
const PASTE_SETTLE: Duration = Duration::from_millis(150);

/// Milliseconds since `at`, as the spans are reported.
fn since(at: Instant) -> f32 {
    at.elapsed().as_secs_f32() * 1000.0
}

/// Identifies the daemon on the wire, so a recycled port or a socket some other
/// service has taken over cannot be mistaken for one.
const APP: &str = "myware-voice";
/// The wire version, sent both ways and checked on both sides.
///
/// Bump this when a field changes meaning, not when one is added: an extra field
/// is ignored by an older reader, and every `Reply` field an older client lacks
/// is optional. A mismatch refuses the request and says so, because the danger is
/// a field being read as something it is not.
const PROTOCOL: u32 = 1;

/// A canned transcript, so the state machine can be tested with no model on
/// disk. Read once at startup; a spawned daemon inherits it.
/// Skip the clipboard and the paste, so a test does not need `wl-copy` or a
/// focused window. Delivery itself has its own tests.
/// `MYWARE_VOICE_FAKE_DELIVERY=0` is how a test asks for the real delivery path
/// while still using a fake microphone and model: the shims that stand in for
/// `wl-copy` are cheaper than a real selection owner.
fn fake_delivery_from(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(value) => !value.is_empty() && value != "0",
    }
}

fn fake_delivery() -> bool {
    let value = crate::compat::var("FAKE_DELIVERY");
    fake_delivery_from(value.as_deref())
}

fn fake_model() -> Option<String> {
    crate::compat::var("FAKE_MODEL").filter(|text| !text.is_empty())
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
    /// Whether the current capture commits phrases while the user speaks.
    #[serde(default)]
    pub streaming: bool,
    /// How a transcript is delivered, as configured. Mirrors `config.toml` so
    /// `myware-voice status` can answer "which paste tool is it actually using".
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
    /// The tier in force. `free` unless a valid licence was found at startup.
    #[serde(default)]
    pub tier: Tier,
    /// Why this copy is not Pro, when there is something worth saying.
    #[serde(default)]
    pub licence: Option<String>,
    /// Cost of the last delivery. `None` until one happens.
    #[serde(default)]
    pub timings: Option<Timings>,
}

/// What one delivery cost, in milliseconds.
///
/// The stop press starts the clock; `total_ms` is when the paste chord returned.
/// Recorded per delivery so a change to `chunk_seconds`, the thread count or the
/// model can be argued about with numbers instead of impressions.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Timings {
    /// The recognizer itself, summed over the chunks of this capture.
    pub decode_ms: f32,
    /// Getting the transcript onto the clipboard. `None` when delivery was
    /// skipped, which is a different thing from a copy that took no time.
    pub copy_ms: Option<f32>,
    /// The paste chord. `None` for the same reason.
    pub paste_ms: Option<f32>,
    /// Stop press to the chord returning: queueing, resampling, normalising,
    /// decoding, copying and pasting.
    pub total_ms: f32,
}

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
            streaming: false,
            paste: String::new(),
            config_revision: 0,
            config_error: None,
            last_error: None,
            tier: Tier::Free,
            licence: None,
            timings: None,
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
        /// How long the recognizer took, for the delivery report.
        decode_ms: f32,
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
    /// The client's wire version. Absent from a client older than the field,
    /// which is treated as version 1 rather than refused: refusing an old client
    /// is a worse failure than answering it.
    #[serde(default)]
    protocol: Option<u32>,
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
    /// Whether the current capture commits phrases while the user speaks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    /// The tier in force, and why it is not Pro when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub licence: Option<String>,
    /// Cost of the last delivery, for `myware-voice status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<Timings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One request/response round trip.
pub fn request(command: &str) -> Result<Reply> {
    let mut stream = transport::connect()?;
    let mut body = serde_json::json!({ "cmd": command, "protocol": PROTOCOL });
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
    // The other direction of the same check: an older daemon answering a newer
    // client. Its fields may mean something else, so nothing is trusted from it.
    if let Some(version) = reply.protocol {
        if version != PROTOCOL {
            anyhow::bail!(
                "the running daemon speaks protocol {version}, this client speaks {PROTOCOL}; \
                 upgrade or restart it with `myware-voice quit`"
            );
        }
    }
    Ok(reply)
}

/// Send `toggle` to the daemon.
///
/// Starting the daemon is not this command's job: a press with nothing listening
/// reports that instead of racing the login session to launch a second daemon.
pub fn toggle() -> Result<Reply> {
    request("toggle")
}

/// Send `toggle-stream` to the daemon, on the same terms.
pub fn toggle_stream() -> Result<Reply> {
    request("toggle-stream")
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
    /// Read once at startup. A licence dropped in while the daemon runs takes
    /// effect after a restart, like a hotkey change.
    tier: Tier,
    /// When the stop press arrived, for the delivery spans.
    stopped_at: Option<Instant>,
    /// Set by a spoken "quit myware" or the equivalent.
    quit_requested: bool,
    /// The Linux listening popup, while a capture is running.
    #[cfg(target_os = "linux")]
    popup: Option<crate::popup::Popup>,
    /// What the clipboard held when this capture started, if the policy is to put
    /// it back once the transcript has been pasted.
    pending_restore: Option<String>,
    /// When to put it back. Set once the paste chord has been sent, so the target
    /// application has time to read the selection before it is replaced.
    restore_at: Option<Instant>,
    /// What we last wrote to the clipboard, so a restore can tell our text from
    /// something the user copied since.
    last_payload: Option<String>,
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
            Event::Decoded {
                generation,
                text,
                decode_ms,
            } => self.delivered(generation, text, decode_ms),
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

    /// Run a whole-utterance command. The transcript is discarded either way.
    fn perform(&mut self, command: crate::text::Command) {
        note(&format!("command: {}", command.arg()));
        match command {
            crate::text::Command::Discard => {
                if !self.fake_delivery {
                    inject::notify("Discarded", "Nothing was pasted");
                }
            }
            // The event loop checks this after every event, so the socket, the
            // tray and a spoken command all stop the daemon the same way.
            crate::text::Command::Quit => self.quit_requested = true,
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
            inject::notify("MyWare Voice did not transcribe", reason);
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
                .map(|recorder| recorder.frames())
                .unwrap_or(0);
            self.streaming = true;
            self.stream_committed = committed;
            self.stream_focus = inject::focus();
            self.stream_text.clear();
            if self.word_by_word() {
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

    /// Whether the word-by-word engine may be used.
    ///
    /// It is the Pro feature. Without a licence streaming still works: the phrase
    /// chunker is the same session with a different handoff, so the fallback is a
    /// working mode rather than a refusal.
    fn word_by_word(&self) -> bool {
        self.config.stream_online && self.tier.is_pro()
    }

    fn start_recording(&mut self, streaming: bool) -> Result<()> {
        // An edit that arrived mid-capture lands here, before anything reads the
        // config for the new recording.
        if let Some(config) = self.pending_config.take() {
            self.apply_config(config);
        }
        // Refuse before the microphone opens, not after the user has finished
        // talking: without `wl-copy` there is no way for a transcript to leave
        // the daemon, so recording would be a promise it cannot keep. The reason
        // reaches the user through the failure path.
        if let Some(problem) = inject::unavailable() {
            bail!("{problem}");
        }
        let recorder = Recorder::start(self.config.device.as_deref())?;
        let device = recorder.device().to_string();
        // Bumping the generation is what cancels a decode that is still running.
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.unloaded = false;
        self.last_activity = Instant::now();
        self.started = Instant::now();
        self.recorder = Some(recorder);
        // The clipboard is handled after the microphone is open: a saved selection
        // is worth a few spawns, but never worth delaying the first syllable.
        self.restore_clipboard();
        self.pending_restore = if self.config.clipboard == Clipboard::Keep {
            inject::read_clipboard()
        } else {
            None
        };
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
        // Started after the recorder, so the first frame already has a level.
        #[cfg(target_os = "linux")]
        {
            self.popup = crate::popup::Popup::start();
        }
        if streaming && self.config.stream_online && !self.tier.is_pro() {
            note("stream_online needs a Pro licence; streaming will use phrase chunks");
        }
        if streaming && self.word_by_word() {
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
        // The clock for the delivery spans starts here, at the stop press, not
        // when the decoder gets round to it.
        self.stopped_at = Some(Instant::now());
        // The stop press is otherwise silent until the transcript lands, and the
        // decode in between is the longest wait in the cycle.
        if self.config.tones {
            tone::play(Cue::Stop);
        }
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
        // What the decoder is about to be given, so a transcript that came from
        // near-silence is a measurement rather than a mystery.
        let rms = {
            let energy: f32 = samples.iter().map(|sample| sample * sample).sum();
            (energy / samples.len().max(1) as f32).sqrt()
        };
        note(&format!("capture {seconds:.1}s rms {rms:.4}"));
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
        let Some((raw, rate, frames)) = self
            .recorder
            .as_ref()
            .map(|recorder| recorder.mono_since(self.stream_committed))
        else {
            return;
        };

        let elapsed = frames as f32 / rate.max(1) as f32;
        let committed = self.stream_committed as f32 / rate.max(1) as f32;
        let target = self.config.stream_seconds.max(0.5);
        if elapsed - committed < target {
            return;
        }
        if self.stream_committed >= frames || raw.is_empty() {
            return;
        }

        let Ok(mut chunk) = audio::resample(&raw, rate, audio::SAMPLE_RATE) else {
            return;
        };
        if chunk.len() as f32 / (audio::SAMPLE_RATE as f32) < 0.25 {
            return;
        }
        let forced = elapsed - committed >= target * 2.5;
        if !forced && !silent_tail(&raw, rate) {
            return;
        }

        audio::normalize_loudness(&mut chunk);
        self.stream_committed = frames;
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
        // Before the branch: both streaming paths end at the stop press, and the
        // cue belongs to the press rather than to how the tail is decoded.
        if self.config.tones {
            tone::play(Cue::Stop);
        }
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
        let Some((raw, rate, frames)) = self
            .recorder
            .as_ref()
            .map(|recorder| recorder.mono_since(self.online_fed))
        else {
            return Ok(());
        };
        if self.online_fed >= frames {
            return Ok(());
        }
        // 50 ms of audio keeps the hypothesis moving without running the
        // feature frontend on every poll.
        if raw.len() < (rate as usize / 20).max(160) {
            return Ok(());
        }

        let started = Instant::now();
        let (hypothesis, endpoint) = {
            let Some(stream) = self.online.as_ref() else {
                return Ok(());
            };
            // The streaming API takes the device rate and resamples internally,
            // so feeding it raw avoids padding a 16 kHz resampler at every
            // chunk edge.
            stream.accept(&raw, rate)?;
            (stream.text()?, stream.endpoint())
        };
        // This runs on the event-loop thread, which is also what answers the
        // keybind. A slow accept delays the next press, so it is measured: this
        // is the number that decides whether the online decode has to move off
        // this thread.
        let accept = started.elapsed();
        if accept > Duration::from_millis(10) {
            log::write(&format!(
                "streaming: accept took {:.1} ms",
                accept.as_secs_f64() * 1000.0
            ));
        }
        let hypothesis = online_case(&hypothesis);
        self.online_fed = frames;

        let since_handoff =
            (self.online_fed - self.online_segment_start) as f32 / rate.max(1) as f32;
        let target = self.config.stream_segment_seconds.max(2.0);
        // Prefer to hand off at a pause, but never let one head grow past 1.5x
        // the target when a monologue has no pause at all.
        let handoff =
            since_handoff >= target && (silent_tail(&raw, rate) || since_handoff >= target * 1.5);

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
            inject::notify("Streaming stopped", &focus_changed_message(now.is_some()));
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

        // Only the mid-sentence-safe part: a snippet or a command must see the
        // whole utterance, not one phrase of it.
        let text = crate::text::cleanup_only(text, &self.config)
            .trim()
            .to_string();
        if text.is_empty() {
            return Ok(());
        }

        self.stream_text.push_str(&text);
        self.stream_text.push(' ');

        // Recorded before the delivery branch, so a fake-delivery test sees the
        // same status a real one would report.
        if let Ok(mut status) = self.status.lock() {
            status.last_text = Some(text.clone());
        }

        if self.fake_delivery {
            log::write("streamed (fake delivery)");
            return Ok(());
        }

        inject::copy(&payload)?;
        self.last_payload = Some(payload.clone());
        match inject::paste(self.config.paste, now.as_ref()) {
            Ok(crate::config::PasteMethod::None) => {
                inject::notify("Streaming stopped", "No paste tool available");
                return Err(anyhow!("no paste tool"));
            }
            // The session's last chunk is the end of the paste run, so the
            // selection that was there before it started comes back then. Earlier
            // chunks are followed by another copy within milliseconds anyway.
            Ok(_) if final_chunk => self.schedule_restore(),
            Ok(_) => {}
            Err(error) => return Err(error).context("stream paste"),
        }
        self.last_activity = Instant::now();
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

    fn delivered(&mut self, generation: u64, text: String, decode_ms: f32) -> Result<()> {
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

        // The cleanup and replacement pass, then a command if the whole utterance
        // was one. A command is not a transcript: nothing is copied or pasted.
        let text = match crate::text::apply(&text, &self.config) {
            crate::text::Outcome::Text(text) => text,
            crate::text::Outcome::Command(command) => {
                self.perform(command);
                self.set(Phase::Idle);
                return Ok(());
            }
        };
        if text.trim().is_empty() {
            inject::notify(
                "Nothing to paste",
                "The cleanup pass emptied the transcript",
            );
            self.set(Phase::Idle);
            return Ok(());
        }

        // A terminal gets one line. `copy()` trims only trailing newlines, and the
        // cleanup pass only collapses internal whitespace by accident — with
        // `cleanup = false` nothing else would, and a newline pasted into a shell
        // runs the next word as a command.
        let text = if self.focus.as_ref().is_some_and(|focus| focus.is_terminal()) {
            crate::text::flatten(&text)
        } else {
            text
        };

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
            self.record_timings(decode_ms, None, None);
            self.clear_error();
            self.set(Phase::Idle);
            return Ok(());
        }

        let copy_at = Instant::now();
        inject::copy(&text)?;
        self.last_payload = Some(text.clone());
        let copy_ms = since(copy_at);
        if self.config.history {
            if let Err(error) = crate::history::append(text.trim_end()) {
                note(&format!("history: {error:#}"));
            }
            let _ = crate::history::prune(self.config.history_days);
        }

        let focus_now = inject::focus();
        if !same_focus(self.focus.as_ref(), focus_now.as_ref()) {
            inject::notify(
                "Transcript copied",
                &focus_changed_message(focus_now.is_some()),
            );
            self.record_timings(decode_ms, Some(copy_ms), None);
            self.set(Phase::Idle);
            return Ok(());
        }

        let paste_at = Instant::now();
        let method = inject::paste(self.config.paste, focus_now.as_ref());
        let paste_ms = since(paste_at);
        match method {
            Ok(crate::config::PasteMethod::None) => {
                inject::notify("Transcript copied", "No paste tool available");
            }
            // The text is in the window; the copy of it is no longer needed.
            Ok(_) => self.schedule_restore(),
            Err(error) => {
                note_error(&format!("paste failed: {error:#}"));
                inject::notify("Transcript copied", "Paste failed; paste it yourself");
            }
        }
        if self.config.tones {
            tone::play(Cue::Done);
        }
        let timings = self.record_timings(decode_ms, Some(copy_ms), Some(paste_ms));
        // The count, the spans and nothing else: the transcript itself is not the
        // log's business.
        log::write(&format!(
            "delivered {} chars · decode {:.0} ms · copy {:.0} ms · paste {:.0} ms · total {:.0} ms",
            text.trim_end().chars().count(),
            timings.decode_ms,
            copy_ms,
            paste_ms,
            timings.total_ms
        ));
        self.clear_error();
        self.set(Phase::Idle);
        Ok(())
    }

    /// Store the spans of this delivery and return them.
    ///
    /// `stopped_at` is taken rather than read, so a delivery that never happened
    /// cannot leave a clock running into the next one.
    fn record_timings(
        &mut self,
        decode_ms: f32,
        copy_ms: Option<f32>,
        paste_ms: Option<f32>,
    ) -> Timings {
        let total_ms = self.stopped_at.take().map(since).unwrap_or_default();
        let timings = Timings {
            decode_ms,
            copy_ms,
            paste_ms,
            total_ms,
        };
        if let Ok(mut status) = self.status.lock() {
            status.timings = Some(timings);
        }
        timings
    }

    /// Put the previous clipboard back, if it is still ours to put back.
    ///
    /// Runs `PASTE_SETTLE` after a successful paste, not on the next press: the
    /// transcript is a means of getting text into a window, so the selection the
    /// user had is returned as soon as the paste that needed it is over. A capture
    /// that never pasted — `paste = none`, a failed chord, a window that moved, or
    /// no transcript at all — leaves the text where the user can collect it; the
    /// next capture puts the old selection back instead.
    fn restore_clipboard(&mut self) {
        self.restore_at = None;
        let Some(previous) = self.pending_restore.take() else {
            return;
        };
        let delivered = self.last_payload.clone();
        let current = inject::read_clipboard();
        if !keep_clipboard(current.as_deref(), delivered.as_deref()) {
            return;
        }
        match inject::set_clipboard(&previous) {
            Ok(()) => log::write("clipboard restored"),
            Err(error) => note(&format!("clipboard: could not restore: {error:#}")),
        }
    }

    /// Arm the restore for after the target application has read the selection.
    ///
    /// Only a real paste justifies replacing the clipboard: the transcript has
    /// reached the window, so the copy of it is dead weight. Called by both
    /// delivery paths, once the chord has been sent.
    fn schedule_restore(&mut self) {
        if self.pending_restore.is_some() {
            self.restore_at = Some(Instant::now() + PASTE_SETTLE);
        }
    }

    /// The tick side of `schedule_restore`.
    fn restore_due(&mut self) {
        if self.restore_at.is_some_and(|at| Instant::now() >= at) {
            self.restore_clipboard();
        }
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
        // The popup lives exactly as long as the capture, so every path that
        // leaves Recording closes it, including a failure.
        #[cfg(target_os = "linux")]
        if phase != Phase::Recording {
            if let Some(popup) = self.popup.take() {
                popup.stop();
            }
        }
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
            status.streaming = self.streaming;
        }
    }

    /// Feed the popup one frame. Cheap enough for the tick, and silent when there
    /// is nothing to show.
    #[cfg(target_os = "linux")]
    fn popup_tick(&mut self) {
        let Some(popup) = self.popup.as_mut() else {
            return;
        };
        let snapshot = self.status.lock().map(|status| status.clone());
        match snapshot {
            Ok(status) if status.phase == Phase::Recording => popup.send(&status),
            _ => {}
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn popup_tick(&mut self) {}

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
                // The spectrum is not computed any more: both popups fill a glyph
                // from the level alone, and 36 window transforms a second for
                // nobody is exactly the kind of work this project measures away.
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
/// What to tell the user when the paste did not happen.
///
/// Two different situations that both end in "copied, not pasted": the window
/// moved, or this desktop cannot say what the focused window is. The second one
/// is worth naming, because the fix is a compositor the daemon can read.
fn focus_changed_message(readable: bool) -> String {
    if readable {
        "Focus changed; paste it yourself".to_string()
    } else {
        "Could not read the focused window; paste it yourself".to_string()
    }
}

/// Is the clipboard still holding what we put there?
///
/// If the user copied something in the meantime it is theirs, and restoring would
/// throw it away. Not knowing what is on the clipboard also means not touching it.
fn keep_clipboard(current: Option<&str>, delivered: Option<&str>) -> bool {
    match (current, delivered) {
        (Some(current), Some(delivered)) => current == delivered,
        _ => false,
    }
}

fn same_focus(captured: Option<&Focus>, now: Option<&Focus>) -> bool {
    match (captured, now) {
        (Some(captured), Some(now)) => captured == now,
        // A window that could not be read is not a window that can be trusted.
        // Pasting blind is how a transcript ends up in whichever window happened
        // to have focus; it is already on the clipboard, so copy and say so.
        _ => false,
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

/// Find the model location, then run. This is what both `myware-voice daemon` and
/// the `myware-voice-daemon` binary call; the configuration is read inside `run`,
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
    let model = if crate::compat::var_os("FAKE_MODEL").is_some() {
        PathBuf::new()
    } else {
        crate::model::resolve_dir()?
    };
    run(model)
}

/// Run the daemon in the foreground until it is killed.
///
/// The config is resolved and read here rather than passed in: the tick re-reads
/// it whenever the file changes, so this is the one place that decides where it
/// lives, and a file that will not parse must leave the daemon running on the
/// defaults instead of refusing to start.
pub fn run(model_dir: PathBuf) -> Result<()> {
    let config_path = Config::path()?;
    let (config, config_error) = match Config::load_from(&config_path) {
        Ok(config) => (config, None),
        Err(error) => {
            let message = format!("config not read; using the defaults: {error:#}");
            note_error(&message);
            (Config::default(), Some(message))
        }
    };
    // Clamped: a zero or negative count is meaningless to ONNX Runtime, and a
    // hundred threads on a laptop makes the decode slower, not faster.
    let threads = config.threads.clamp(1, 16);
    let entitlement = licence::load();
    if let Some(note) = &entitlement.note {
        note_error(&format!("licence: {note}"));
    }
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
        view.tier = entitlement.tier;
        view.licence = entitlement.note.clone();
    }

    if let Some(problem) = &model_problem {
        // The one failure worth interrupting for: without a model nothing works,
        // and the fix is a command the user has to be told.
        if !fake_delivery() {
            inject::notify("MyWare Voice has no model", problem);
        }
    }

    // A tray is a convenience: on Linux its absence (no StatusNotifier host) must
    // not stop the daemon, which is why that implementation logs instead of
    // failing. On Windows the icon is also the only way back to the window, so a
    // failure there is still fatal.
    #[cfg(windows)]
    crate::tray::spawn(events_tx.clone())?;
    #[cfg(target_os = "linux")]
    {
        let _ = crate::tray::spawn(events_tx.clone());
    }

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
        tier: entitlement.tier,
        stopped_at: None,
        quit_requested: false,
        #[cfg(target_os = "linux")]
        popup: None,
        pending_restore: None,
        restore_at: None,
        last_payload: None,
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
                if daemon.quit_requested {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                daemon.reload_config();
                daemon.refresh_seconds();
                daemon.popup_tick();
                daemon.stream_tick();
                daemon.unload_if_idle();
                daemon.enforce_max_capture();
                daemon.restore_due();
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

    // A different wire version means one side is another build, so a field may
    // mean something it does not. Nothing is executed; the reason is the reply.
    if let Some(version) = request.protocol {
        if version != PROTOCOL {
            return refuse(
                &mut *stream,
                format!(
                    "this daemon speaks protocol {PROTOCOL}, the client speaks {version}; \
                     upgrade or restart it with `myware-voice quit`"
                ),
            );
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
                streaming: None,
                tier: None,
                licence: None,
                timings: None,
                error: None,
            }
        }
        // Streaming is reachable on Linux only through here: the compositor owns
        // the keybind, so there is no registered chord to start it the way the
        // Windows hotkey does.
        "toggle-stream" => {
            let _ = events.send(Event::ToggleStream);
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
                streaming: None,
                tier: None,
                licence: None,
                timings: None,
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
                streaming: None,
                tier: None,
                licence: None,
                timings: None,
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
                streaming: Some(snapshot.streaming),
                tier: Some(snapshot.tier.arg().to_string()),
                licence: snapshot.licence,
                timings: snapshot.timings,
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
            streaming: None,
            tier: None,
            licence: None,
            timings: None,
            error: Some(format!("unknown command {other:?}")),
        },
    };

    serde_json::to_writer(&mut stream, &reply)?;
    stream.write_all(b"\n")?;
    Ok(())
}

/// How long `MYWARE_VOICE_FAKE_DECODE=slow` spends per job. Long enough that a
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
    /// `MYWARE_VOICE_FAKE_DECODE`: `panic` fails the first decode, `slow` delays
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
                        decode_ms: 20.0,
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
                let decode_at = Instant::now();
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
                    decode_ms: decode_at.elapsed().as_secs_f32() * 1000.0,
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

/// Answer with nothing but why: no command runs, and the version is reported so
/// the other side can say what it found.
fn refuse(stream: &mut dyn Stream, message: String) -> Result<()> {
    let reply = Reply {
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
        streaming: None,
        tier: None,
        licence: None,
        timings: None,
        error: Some(message),
    };
    serde_json::to_writer(&mut *stream, &reply)?;
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
    let fake_decode = crate::compat::var("FAKE_DECODE");
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

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    #[test]
    fn the_clipboard_goes_back_only_while_it_is_still_ours() {
        assert!(keep_clipboard(Some("hello "), Some("hello ")));
        // The user copied something between the paste and the next press.
        assert!(!keep_clipboard(Some("something else"), Some("hello ")));
        // Nothing read, or nothing delivered: leave it alone.
        assert!(!keep_clipboard(None, Some("hello ")));
        assert!(!keep_clipboard(Some("hello "), None));
    }

    #[test]
    fn fake_delivery_can_be_turned_off_with_a_zero() {
        assert!(!fake_delivery_from(None));
        assert!(!fake_delivery_from(Some("")));
        assert!(!fake_delivery_from(Some("0")));
        assert!(fake_delivery_from(Some("1")));
    }
}

#[cfg(all(test, unix))]
mod focus_tests {
    use super::*;

    fn at(app: &str) -> Focus {
        Focus {
            app_id: app.to_string(),
            title: "window".to_string(),
            window_id: Some("1".to_string()),
        }
    }

    fn at_window(app: &str, window: &str, title: &str) -> Focus {
        Focus {
            app_id: app.to_string(),
            title: title.to_string(),
            window_id: Some(window.to_string()),
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
    fn a_title_change_is_not_a_focus_change() {
        // A shell prompt rewrites the title after every command; refusing the
        // paste because of that was the reason this comparison changed.
        let before = at_window("foot", "7", "~/repos");
        let after = at_window("foot", "7", "~/repos (git)-[main]");
        assert!(same_focus(Some(&before), Some(&after)));
    }

    #[test]
    fn a_second_window_of_the_same_application_is_a_focus_change() {
        let left = at_window("editor", "7", "notes");
        let right = at_window("editor", "9", "notes");
        assert!(!same_focus(Some(&left), Some(&right)));
    }

    #[test]
    fn a_window_we_could_not_read_is_not_pasted_into() {
        // No provider, or the compositor did not answer. Pasting blind is how a
        // transcript lands in a chat window; the clipboard already has it.
        assert!(!same_focus(None, None));
    }

    #[test]
    fn losing_focus_after_capturing_it_does_not_paste() {
        assert!(!same_focus(Some(&at("editor")), None));
    }
}
