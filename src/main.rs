use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use hippo_voice::audio;
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
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Doctor => doctor(),
        Command::Transcribe {
            path,
            json,
            threads,
        } => transcribe(&path, json, threads),
    }
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
