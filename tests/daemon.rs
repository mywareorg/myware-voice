//! End to end over the real transport: a spawned daemon, a fake microphone and a
//! fake recognizer.
//!
//! No model, no device and no display server, so this runs on any Linux CI box.
//! The daemon is a separate process because that is the only way to exercise the
//! socket, the detached spawn path and the process lifecycle together.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hippo_voice::audio;
use hippo_voice::daemon;

/// The client resolves the endpoint from the process environment, which is
/// global. One test at a time keeps the two round trips from fighting over it.
static ENV: Mutex<()> = Mutex::new(());

struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("hippo-voice-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create the test directory");
        Self(root)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    condition()
}

/// One full press-record-press cycle, returning whatever was delivered.
fn round_trip(name: &str, extra: &[(&str, &str)]) -> Option<String> {
    let temp = Temp::new(name);
    let runtime = temp.0.join("run");
    let config_home = temp.0.join("config");
    let state_home = temp.0.join("state");
    std::fs::create_dir_all(config_home.join("hippo-voice")).unwrap();
    std::fs::create_dir_all(&state_home).unwrap();
    std::fs::write(
        config_home.join("hippo-voice/config.toml"),
        "paste = \"none\"\ntones = false\nhistory = false\n",
    )
    .unwrap();

    // Half a second of tone, which clears the minimum-utterance gate.
    let wav = temp.0.join("speech.wav");
    let samples: Vec<f32> = (0..8_000)
        .map(|index| (index as f32 * 0.05).sin() * 0.3)
        .collect();
    audio::write_wav(&wav, &samples, audio::SAMPLE_RATE).unwrap();

    // The client in this process resolves the endpoint from these.
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    std::env::set_var("XDG_STATE_HOME", &state_home);

    let mut command = Command::new(env!("CARGO_BIN_EXE_hippo-voice"));
    command
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("HIPPO_VOICE_FAKE_AUDIO", &wav)
        .env("HIPPO_VOICE_FAKE_MODEL", "hello from the test")
        .env("HIPPO_VOICE_FAKE_DELIVERY", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in extra {
        command.env(key, value);
    }

    let _daemon = Daemon(command.spawn().expect("spawn the daemon"));

    assert!(
        wait_until(
            || daemon::request("status").is_ok(),
            Duration::from_secs(15)
        ),
        "the daemon never answered on its socket"
    );

    let reply = daemon::request("toggle").expect("toggle on");
    assert!(reply.ok);
    assert!(
        wait_until(
            || daemon::request("status")
                .ok()
                .map(|status| status.phase.as_deref() == Some("recording"))
                .unwrap_or(false),
            Duration::from_secs(5)
        ),
        "the daemon never started recording"
    );

    std::thread::sleep(Duration::from_millis(150));
    daemon::request("toggle").expect("toggle off");

    assert!(
        wait_until(
            || daemon::request("status").ok().is_some_and(|status| {
                status.text.is_some() && status.phase.as_deref() == Some("idle")
            }),
            Duration::from_secs(15)
        ),
        "the daemon never finished delivering"
    );

    let status = daemon::request("status").expect("status");
    assert_eq!(status.phase.as_deref(), Some("idle"));
    status.text
}

#[test]
fn toggle_records_decodes_and_reports_a_transcript() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let text = round_trip("daemon", &[]);
    assert_eq!(text.as_deref(), Some("hello from the test"));
}

#[test]
fn a_device_that_dies_mid_capture_still_delivers() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let text = round_trip("salvage", &[("HIPPO_VOICE_FAKE_AUDIO_FAILS", "1")]);
    assert_eq!(
        text.as_deref(),
        Some("hello from the test"),
        "the captured audio was discarded because the device reported a failure"
    );
}
