use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use hippo_voice::audio;
use hippo_voice::capture;
use hippo_voice::config::Config;
use hippo_voice::daemon;
use hippo_voice::history;
use hippo_voice::inject;
use hippo_voice::log;
use hippo_voice::model;
use hippo_voice::sherpa::Sherpa;

#[derive(Parser)]
#[command(
    name = "hippo-voice",
    version,
    about = "Push-to-talk dictation. Speak; the transcript lands where you were typing."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum ModelCommand {
    /// Show the package, its files and where it lives.
    Status {
        /// Show the streaming model instead of the batch model.
        #[arg(long)]
        streaming: bool,
    },
    /// Hash every file and report a corrupt one.
    Verify {
        #[arg(long)]
        streaming: bool,
    },
    /// Fetch what is missing, resuming partial files.
    Download {
        #[arg(long)]
        streaming: bool,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Report what the app can see, and what is missing.
    Doctor,
    /// Transcribe an audio file and print the text.
    Transcribe {
        /// WAV file to transcribe.
        path: PathBuf,
        /// Machine-readable output with timings.
        #[arg(long)]
        json: bool,
        /// Threads for the recognizer.
        #[arg(long, default_value_t = 4)]
        threads: i32,
    },
    /// Run the resident daemon in the foreground.
    Daemon,
    /// Start or stop recording. This is what the keybind runs.
    Toggle,
    /// Start or stop dictation that pastes phrases while you speak.
    Stream,
    /// Print the daemon's state.
    Status,
    /// Stop the daemon.
    Quit,
    /// Show the daemon log, and where it is.
    Log {
        #[arg(long, default_value_t = 40)]
        lines: usize,
    },
    /// Show the opt-in transcript history.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Delete the history file.
        #[arg(long)]
        clear: bool,
    },
    /// Inspect, verify or fetch the speech model.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Capture from the microphone and write a WAV.
    Record {
        /// Where to write the recording.
        out: PathBuf,
        /// How long to capture.
        #[arg(long, default_value_t = 5.0)]
        seconds: f32,
        /// Input device name fragment; defaults to the host default.
        #[arg(long)]
        device: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Doctor => doctor(),
        Command::Transcribe {
            path,
            json,
            threads,
        } => transcribe(&path, json, threads),
        Command::Record {
            out,
            seconds,
            device,
        } => record(&out, seconds, device.as_deref()),
        Command::Daemon => daemon::run_process(),
        Command::Model { command } => model_command(command),
        Command::Log { lines } => {
            println!("{}", log::path()?.display());
            for line in log::tail(lines)? {
                println!("{line}");
            }
            Ok(())
        }
        Command::History { limit, clear } => {
            if clear {
                history::clear()?;
                println!("history cleared");
                return Ok(());
            }
            let config = Config::load()?;
            if !config.history {
                println!("history is off; set `history = true` in config.toml");
                return Ok(());
            }
            let entries = history::recent(config.history_days)?;
            for entry in entries.iter().rev().take(limit).rev() {
                println!("{}  {}", log::stamp(entry.epoch), entry.text);
            }
            Ok(())
        }
        Command::Quit => match daemon::request("quit") {
            Ok(_) => {
                println!("stopped");
                Ok(())
            }
            Err(error) if hippo_voice::transport::is_unreachable(&error) => {
                println!("not running");
                Ok(())
            }
            Err(error) => Err(error),
        },
        Command::Toggle => {
            let reply = daemon::toggle()?;
            match reply.phase {
                Some(phase) => println!("{phase}"),
                None => println!("ok"),
            }
            Ok(())
        }
        Command::Stream => {
            let reply = daemon::toggle_stream()?;
            if !reply.ok {
                anyhow::bail!(reply.error.unwrap_or_else(|| "daemon error".into()));
            }
            println!("ok");
            Ok(())
        }
        Command::Status => {
            let reply = match daemon::request("status") {
                Ok(reply) => reply,
                Err(error) if hippo_voice::transport::is_unreachable(&error) => {
                    println!("not running");
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            if !reply.ok {
                anyhow::bail!(reply.error.unwrap_or_else(|| "daemon error".into()));
            }
            println!(
                "{} {:.1}s{}",
                reply.phase.unwrap_or_default(),
                reply.seconds.unwrap_or_default(),
                reply
                    .device
                    .filter(|device| !device.is_empty())
                    .map(|device| format!(" · {device}"))
                    .unwrap_or_default()
            );
            if let Some(hotkey) = reply.hotkey {
                println!("hotkey: {hotkey}");
            }
            if let Some(paste) = reply.paste {
                println!("paste: {paste}");
            }
            if reply.streaming == Some(true) {
                println!("streaming: on");
            }
            // Revision zero is the file as it was at startup; only a reload is
            // worth saying out loud.
            if let Some(revision) = reply.config_revision.filter(|revision| *revision > 0) {
                println!("config: revision {revision}");
            }
            if let Some(error) = reply.config_error {
                println!("config: {error}");
            }
            if let Some(error) = reply.last_error {
                println!("last error: {error}");
            }
            if let Some(text) = reply.text {
                println!("last: {text}");
            }
            Ok(())
        }
    }
}

fn record(out: &Path, seconds: f32, device: Option<&str>) -> Result<()> {
    let recorder = capture::Recorder::start(device)?;
    eprintln!("capturing for {seconds:.1}s from {}", recorder.device());

    let deadline = Instant::now() + std::time::Duration::from_secs_f32(seconds.max(0.1));
    while Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if let Some(failure) = recorder.failure() {
            anyhow::bail!("audio device failed: {failure}");
        }
    }

    let native = recorder.rate();
    let (samples, rate) = recorder.finish_mono();
    let captured = samples.len() as f32 / rate as f32;
    let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));

    let mut samples = audio::resample(&samples, rate, audio::SAMPLE_RATE)?;
    let gain = audio::normalize_loudness(&mut samples);
    audio::write_wav(out, &samples, audio::SAMPLE_RATE)?;

    eprintln!(
        "wrote {} · {captured:.2}s at {native} Hz · peak {peak:.3} · gain {gain:.2}",
        out.display()
    );
    Ok(())
}

/// Where the Parakeet package lives.
fn model_dir() -> Result<PathBuf> {
    model::resolve_dir()
}

fn doctor() -> Result<()> {
    println!("hippo-voice {}", env!("CARGO_PKG_VERSION"));

    match Sherpa::open() {
        Ok(sherpa) => {
            println!("sherpa-onnx   {}", sherpa.version());
            println!("  library     {}", sherpa.path().display());
        }
        Err(error) => {
            println!("sherpa-onnx   MISSING");
            println!("  {error:#}");
            println!("  fix: scripts/fetch-sherpa.sh");
        }
    }

    match model_dir() {
        Ok(dir) => {
            let rows = model::inspect(&dir, false)?;
            let missing: Vec<&str> = rows
                .iter()
                .filter(|(_, state, _)| *state != model::State::Present)
                .map(|(file, _, _)| file.name.as_str())
                .collect();
            if missing.is_empty() {
                println!("model         {}", dir.display());
            } else {
                println!("model         INCOMPLETE at {}", dir.display());
                println!("  missing     {}", missing.join(", "));
                println!("  fix         hippo-voice model download");
            }
        }
        Err(error) => {
            println!("model         MISSING");
            println!("  {error}");
        }
    }

    match model::streaming_dir() {
        Ok(dir) => match model::inspect_streaming(&dir, false) {
            Ok(rows) => {
                let missing: Vec<&str> = rows
                    .iter()
                    .filter(|(_, state, _)| *state != model::State::Present)
                    .map(|(file, _, _)| file.name.as_str())
                    .collect();
                if missing.is_empty() {
                    println!("streaming     {}", dir.display());
                } else {
                    println!("streaming     INCOMPLETE at {}", dir.display());
                    println!(
                        "  fix         hippo-voice model download --streaming (falls back to phrase chunks)"
                    );
                }
            }
            Err(error) => println!("streaming     unreadable: {error}"),
        },
        Err(error) => println!("streaming     MISSING: {error}"),
    }

    println!("session       {}", session_name());
    // Load-bearing now that an unreadable window means "copied, not pasted": the
    // user needs to know whether this desktop can say what is focused.
    println!(
        "focus         {}",
        if inject::focus().is_some() {
            "readable"
        } else {
            "not readable: transcripts are copied, not pasted"
        }
    );

    match capture::input_devices() {
        Ok(devices) if !devices.is_empty() => {
            println!("inputs        {}", devices.len());
            for name in devices {
                println!("  {name}");
            }
        }
        Ok(_) => println!("inputs        none"),
        Err(error) => println!("inputs        unavailable: {error}"),
    }

    let config = Config::load()?;
    println!(
        "paste         {} -> {}",
        config.paste.arg(),
        inject::resolve(config.paste).arg()
    );
    #[cfg(unix)]
    println!(
        "cue player    {}",
        if let Some(path) = inject::find("pw-play") {
            path.display().to_string()
        } else if let Some(path) = inject::find("paplay") {
            path.display().to_string()
        } else {
            "missing (no start/stop cues)".to_string()
        }
    );

    #[cfg(unix)]
    for bin in ["wl-copy", "wl-paste", "wtype", "ydotool", "notify-send"] {
        match inject::find(bin) {
            Some(path) => println!("{bin:<13} {}", path.display()),
            None => println!("{bin:<13} missing"),
        }
    }

    #[cfg(windows)]
    {
        println!("clipboard     built in (arboard)");
        println!("paste         built in (SendInput)");
        println!("hotkey        {}", config.hotkey);
        println!("cue player    not implemented on Windows yet");
    }
    Ok(())
}

fn model_command(command: ModelCommand) -> Result<()> {
    let streaming = match command {
        ModelCommand::Status { streaming }
        | ModelCommand::Verify { streaming }
        | ModelCommand::Download { streaming } => streaming,
    };
    let lock = if streaming {
        model::streaming_lock()?
    } else {
        model::lock()?
    };
    let dir = if streaming {
        model::streaming_dir()?
    } else {
        model::resolve_dir()?
    };
    let rerun = if streaming {
        "hippo-voice model download --streaming"
    } else {
        "hippo-voice model download"
    };
    match command {
        ModelCommand::Status { .. } => {
            println!("package       {} ({})", lock.version, lock.dir);
            println!("licence       {}", lock.license);
            println!("attribution   {}", lock.attribution);
            println!("directory     {}", dir.display());
            let rows = if streaming {
                model::inspect_streaming(&dir, false)?
            } else {
                model::inspect(&dir, false)?
            };
            let mut total = 0u64;
            for (file, state, bytes) in rows {
                total += bytes;
                println!(
                    "  {:<52} {:<8} {} MiB",
                    file.name,
                    state.arg(),
                    bytes / (1 << 20)
                );
            }
            println!("total         {} MiB", total / (1 << 20));
        }
        ModelCommand::Verify { .. } => {
            let rows = if streaming {
                model::inspect_streaming(&dir, true)?
            } else {
                model::inspect(&dir, true)?
            };
            let mut bad = 0;
            for (file, state, bytes) in rows {
                if state != model::State::Present {
                    bad += 1;
                }
                println!(
                    "  {:<52} {:<8} {} MiB",
                    file.name,
                    state.arg(),
                    bytes / (1 << 20)
                );
            }
            if bad > 0 {
                anyhow::bail!("{bad} file(s) need downloading: {rerun}");
            }
            println!("ok");
        }
        ModelCommand::Download { .. } => {
            let mut progress = |name: &str| eprintln!("  {name}");
            let (files, bytes) = if streaming {
                model::download_streaming(&dir, &mut progress)?
            } else {
                model::download(&dir, &mut progress)?
            };
            println!(
                "fetched {files} file(s), {} MiB, into {}",
                bytes / (1 << 20),
                dir.display()
            );
        }
    }
    Ok(())
}

fn session_name() -> &'static str {
    if cfg!(windows) {
        "windows (WASAPI)"
    } else if std::env::var_os("SWAYSOCK").is_some() {
        "sway (Wayland)"
    } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        "Hyprland (Wayland)"
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "Wayland"
    } else if std::env::var_os("DISPLAY").is_some() {
        "X11"
    } else {
        "none"
    }
}

fn transcribe(path: &Path, json: bool, threads: i32) -> Result<()> {
    let dir = model_dir()
        .context("no model directory; set HIPPO_VOICE_MODEL_DIR or run scripts/fetch-model.sh")?;

    let started = Instant::now();
    let (samples, gain) = audio::prepare(path)?;
    let prepared = started.elapsed();

    let sherpa = Sherpa::open()?;
    let load_at = Instant::now();
    let recognizer = sherpa.recognizer(&dir, threads)?;
    let loaded = load_at.elapsed();

    // Long clips are cut at silences: Parakeet degrades badly on one long
    // single pass and grows memory doing it.
    let chunk_seconds = Config::load()
        .map(|config| config.chunk_seconds)
        .unwrap_or(20.0);
    let spans = audio::split_at_silence(&samples, chunk_seconds);

    let decode_at = Instant::now();
    let mut parts = Vec::new();
    for (start, end) in &spans {
        let text = recognizer.transcribe(&samples[*start..*end])?;
        if !text.is_empty() {
            parts.push(text);
        }
    }
    let text = parts.join(" ");
    let decoded = decode_at.elapsed();
    let chunks = spans.len();

    let seconds = samples.len() as f32 / audio::SAMPLE_RATE as f32;
    let rtf = decoded.as_secs_f32() / seconds.max(f32::EPSILON);

    if json {
        let report = serde_json::json!({
            "text": text,
            "seconds": seconds,
            "gain": gain,
            "t_prepare": prepared.as_secs_f64(),
            "t_load": loaded.as_secs_f64(),
            "t_decode": decoded.as_secs_f64(),
            "rtf": rtf,
            "chunks": chunks,
            "peak_rss_mib": peak_rss_mib(),
            "threads": threads,
        });
        println!("{report}");
    } else {
        println!("{text}");
        eprintln!(
            "audio {seconds:.2}s · {chunks} chunk(s) · prepare {:.3}s · load {:.3}s · decode {:.3}s · RTF {rtf:.3} · peak {} MiB",
            prepared.as_secs_f32(),
            loaded.as_secs_f32(),
            decoded.as_secs_f32(),
            peak_rss_mib()
        );
    }
    Ok(())
}

/// Peak resident set, so the warm-model cost is measured rather than guessed.
fn peak_rss_mib() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find(|line| line.starts_with("VmHWM:"))
                    .and_then(|line| line.split_whitespace().nth(1).map(str::to_string))
            })
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb / 1024)
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}
