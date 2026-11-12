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

/// Report a problem to the terminal and to the log file.
fn note(message: &str) {
    eprintln!("{message}");
    log::write(message);
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
}

impl Default for Status {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            seconds: 0.0,
            device: String::new(),
            generation: 0,
            last_text: None,
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Toggle,
    Decoded { generation: u64, text: String },
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
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
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
    Ok(reply)
}

/// Send `toggle`, starting the daemon first if it is not running.
pub fn toggle() -> Result<Reply> {
    match request("toggle") {
        Ok(reply) => Ok(reply),
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
    let log = log::path()?;
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .ok();
    let mut command = std::process::Command::new(exe);
    command.arg("daemon").stdin(std::process::Stdio::null());
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
}

impl Daemon {
    fn handle(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Toggle => self.toggle(),
            Event::Decoded { generation, text } => self.delivered(generation, text),
            Event::Failed(error) => {
                note(&format!("failed: {error}"));
                self.stop_recording();
                self.set(Phase::Idle);
                Ok(())
            }
        }
    }

    fn toggle(&mut self) -> Result<()> {
        match self.status.lock().map(|s| s.phase).unwrap_or(Phase::Idle) {
            Phase::Recording => self.stop_recording_and_decode(),
            // A second press during decode means "I want to talk", not "wait".
            // Starting a capture bumps the generation, which abandons the decode.
            Phase::Decoding => self.start_recording(),
            Phase::Idle => self.start_recording(),
        }
    }

    fn start_recording(&mut self) -> Result<()> {
        let recorder = Recorder::start(self.config.device.as_deref())?;
        let device = recorder.device().to_string();
        // Bumping the generation is what cancels a decode that is still running.
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.unloaded = false;
        self.last_activity = Instant::now();
        self.started = Instant::now();
        self.recorder = Some(recorder);
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
        log::write(&format!("recording on {device}"));
        Ok(())
    }

    fn stop_recording(&mut self) {
        self.recorder = None;
    }

    fn stop_recording_and_decode(&mut self) -> Result<()> {
        let Some(recorder) = self.recorder.take() else {
            self.set(Phase::Idle);
            return Ok(());
        };

        if let Some(failure) = recorder.failure() {
            self.set(Phase::Idle);
            return Err(anyhow!("audio device failed: {failure}"));
        }

        // The window that should receive the paste, recorded before the decode
        // starts rather than after it.
        self.focus = inject::focus();

        let (samples, rate) = recorder.finish_mono();
        let seconds = samples.len() as f32 / rate.max(1) as f32;
        if seconds < 0.3 {
            self.set(Phase::Idle);
            return Ok(());
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

        inject::copy(&text)?;
        if self.config.history {
            if let Err(error) = log::append_history(text.trim_end()) {
                note(&format!("history: {error:#}"));
            }
            let _ = log::prune_history(self.config.history_days);
        }

        let focus_now = inject::focus();
        if focus_now != self.focus {
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
                note(&format!("paste failed: {error:#}"));
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

    fn set(&self, phase: Phase) {
        if let Ok(mut status) = self.status.lock() {
            status.phase = phase;
            status.generation = self.generation.load(Ordering::SeqCst);
            status.seconds = if phase == Phase::Recording {
                self.started.elapsed().as_secs_f32()
            } else {
                0.0
            };
        }
    }

    fn refresh_seconds(&self) {
        if let Ok(mut status) = self.status.lock() {
            if status.phase == Phase::Recording
                && status.generation == self.generation.load(Ordering::SeqCst)
            {
                status.seconds = self.started.elapsed().as_secs_f32();
            }
        }
    }
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

    #[cfg(windows)]
    crate::hotkey::spawn(events_tx.clone())?;

    let decoder_events = events_tx.clone();
    let decoder_generation = Arc::clone(&generation);
    thread::Builder::new()
        .name("decoder".into())
        .spawn(move || {
            decoder_loop(
                jobs_rx,
                decoder_events,
                model_dir,
                threads,
                decoder_generation,
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
    };

    loop {
        match events_rx.recv_timeout(TICK) {
            Ok(Event::Toggle) => {
                if let Err(error) = daemon.handle(Event::Toggle) {
                    note(&format!("toggle: {error:#}"));
                }
            }
            Ok(event) => {
                if let Err(error) = daemon.handle(event) {
                    note(&format!("event: {error:#}"));
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                daemon.refresh_seconds();
                daemon.unload_if_idle();
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
                phase: None,
                seconds: None,
                device: None,
                text: None,
                error: None,
            }
        }
        "status" => {
            let snapshot = status.lock().map(|s| s.clone()).unwrap_or_default();
            Reply {
                ok: true,
                phase: Some(snapshot.phase.arg().to_string()),
                seconds: Some(snapshot.seconds),
                device: Some(snapshot.device),
                text: snapshot.last_text,
                error: None,
            }
        }
        other => Reply {
            ok: false,
            phase: None,
            seconds: None,
            device: None,
            text: None,
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
) {
    let mut loaded: Option<crate::sherpa::Recognizer> = None;

    while let Ok(job) = jobs.recv() {
        match job {
            DecodeJob::Warm => {
                if loaded.is_none() {
                    match load(&model_dir, threads) {
                        Ok(pair) => loaded = Some(pair),
                        Err(error) => {
                            let _ = events.send(Event::Failed(format!("{error:#}")));
                        }
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
