use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use hippo_voice::audio;
use hippo_voice::capture;
use hippo_voice::config::Config;
use hippo_voice::daemon;
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
    /// Print the daemon's state.
    Status,
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
        Command::Daemon => {
            let dir = model_dir().context("no model directory; run scripts/fetch-model.sh")?;
            daemon::run(Config::load()?, dir, 4)
        }
        Command::Toggle => {
            let reply = daemon::toggle()?;
            match reply.phase {
                Some(phase) => println!("{phase}"),
                None => println!("ok"),
            }
            Ok(())
        }
        Command::Status => {
            let reply = daemon::request("status")?;
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
fn model_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HIPPO_VOICE_MODEL_DIR") {
        return Some(PathBuf::from(dir));
    }
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/models/parakeet-v3-int8");
    if dev.is_dir() {
        return Some(dev);
    }
    None
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
        Some(dir) => {
            let missing: Vec<&str> = [
                "encoder.int8.onnx",
                "decoder.int8.onnx",
                "joiner.int8.onnx",
                "tokens.txt",
            ]
            .into_iter()
            .filter(|name| !dir.join(name).is_file())
            .collect();
            if missing.is_empty() {
                println!("model         {}", dir.display());
            } else {
                println!("model         INCOMPLETE at {}", dir.display());
                println!("  missing     {}", missing.join(", "));
            }
        }
        None => {
            println!("model         MISSING");
            println!("  fix: scripts/fetch-model.sh, or set HIPPO_VOICE_MODEL_DIR");
        }
    }

    let which = |bin: &str| {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {bin}"))
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    println!("session       {}", session_name());

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

    for bin in ["wl-copy", "wl-paste", "wtype", "ydotool", "notify-send"] {
        match which(bin) {
            Some(path) => println!("{bin:<13} {path}"),
            None => println!("{bin:<13} missing"),
        }
    }
    Ok(())
}

fn session_name() -> &'static str {
    if std::env::var_os("SWAYSOCK").is_some() {
        "sway (Wayland)"
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

    let decode_at = Instant::now();
    let text = recognizer.transcribe(&samples)?;
    let decoded = decode_at.elapsed();

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
            "peak_rss_mib": peak_rss_mib(),
            "threads": threads,
        });
        println!("{report}");
    } else {
        println!("{text}");
        eprintln!(
            "audio {seconds:.2}s · prepare {:.3}s · load {:.3}s · decode {:.3}s · RTF {rtf:.3} · peak {} MiB",
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
