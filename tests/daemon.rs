//! End to end over the real transport: a spawned daemon, a fake microphone and a
//! fake recognizer.
//!
//! No model, no device and no display server, so this runs on any Linux CI box.
//! The daemon is a separate process because that is the only way to exercise the
//! socket, the detached spawn path and the process lifecycle together.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hippo_dictate::audio;
use hippo_dictate::daemon;

/// The client resolves the endpoint from the process environment, which is
/// global. One test at a time keeps the two round trips from fighting over it.
static ENV: Mutex<()> = Mutex::new(());

struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("hippo-dictate-test-{name}-{}", std::process::id()));
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

/// A running daemon with its own directories, and the config file it read.
struct Fixture {
    /// Dropped last: removes the directories the daemon was pointed at.
    _temp: Temp,
    config_path: PathBuf,
    log_path: PathBuf,
    _daemon: Daemon,
}

impl Fixture {
    /// `model` is the canned transcript for a fake recognizer, or `None` to run
    /// against a real model directory, which the caller points at with `extra`.
    fn start(name: &str, config_text: &str, model: Option<&str>, extra: &[(&str, &str)]) -> Self {
        Self::start_with(name, config_text, model, extra, &tone(0.5))
    }

    /// As `start`, with the fake microphone replayed from `samples`.
    fn start_with(
        name: &str,
        config_text: &str,
        model: Option<&str>,
        extra: &[(&str, &str)],
        samples: &[f32],
    ) -> Self {
        let temp = Temp::new(name);
        let runtime = temp.0.join("run");
        let state_home = temp.0.join("state");
        let config_path = temp.0.join("config.toml");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&state_home).unwrap();
        std::fs::write(&config_path, config_text).unwrap();

        let wav = temp.0.join("speech.wav");
        audio::write_wav(&wav, samples, audio::SAMPLE_RATE).unwrap();

        // The client in this process resolves the endpoint from these.
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("XDG_STATE_HOME", &state_home);
        std::env::set_var("HIPPO_DICTATE_CONFIG", &config_path);

        let mut command = Command::new(env!("CARGO_BIN_EXE_hippo-dictate"));
        command
            .arg("daemon")
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", &state_home)
            // The config file is named outright rather than derived from
            // `XDG_CONFIG_HOME`: this test edits it, and nothing else in the
            // suite should have to care where it lives.
            .env("HIPPO_DICTATE_CONFIG", &config_path)
            .env("HIPPO_DICTATE_FAKE_AUDIO", &wav)
            .env("HIPPO_DICTATE_FAKE_DELIVERY", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(canned) = model {
            command.env("HIPPO_DICTATE_FAKE_MODEL", canned);
        }
        for (key, value) in extra {
            command.env(key, value);
        }

        let daemon = Daemon(command.spawn().expect("spawn the daemon"));
        assert!(
            wait_until(
                || daemon::request("status").is_ok(),
                Duration::from_secs(15)
            ),
            "the daemon never answered on its socket"
        );
        Self {
            _temp: temp,
            config_path,
            // `XDG_STATE_HOME/hippo-dictate/log`, the same resolution the daemon
            // uses, so a test can read what it wrote.
            log_path: state_home.join("hippo-dictate").join("log"),
            _daemon: daemon,
        }
    }

    fn write_config(&self, text: &str) {
        std::fs::write(&self.config_path, text).expect("rewrite the config");
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

/// Tone, which clears the minimum-utterance gate.
fn tone(seconds: f32) -> Vec<f32> {
    let count = (audio::SAMPLE_RATE as f32 * seconds) as usize;
    (0..count)
        .map(|index| (index as f32 * 0.05).sin() * 0.3)
        .collect()
}

/// Tone and then silence: the pause the streaming path waits for before it
/// commits a phrase, so a phrase lands while the key is still held.
fn tone_then_silence(seconds: f32, silence: f32) -> Vec<f32> {
    let mut samples = tone(seconds);
    samples.resize(
        samples.len() + (audio::SAMPLE_RATE as f32 * silence) as usize,
        0.0,
    );
    samples
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
fn deliver_once() -> Option<String> {
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

fn round_trip(name: &str, extra: &[(&str, &str)]) -> Option<String> {
    let _fixture = Fixture::start(
        name,
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("hello from the test"),
        extra,
    );
    deliver_once()
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
    let text = round_trip("salvage", &[("HIPPO_DICTATE_FAKE_AUDIO_FAILS", "1")]);
    assert_eq!(
        text.as_deref(),
        Some("hello from the test"),
        "the captured audio was discarded because the device reported a failure"
    );
}

#[test]
fn an_edited_config_is_picked_up_without_a_restart() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::start(
        "reload",
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("hello from the test"),
        &[],
    );

    let before = daemon::request("status").expect("status");
    assert_eq!(before.paste.as_deref(), Some("none"));
    assert_eq!(before.config_revision, Some(0));

    fixture.write_config("paste = \"ydotool\"\ntones = false\nhistory = false\n");

    assert!(
        wait_until(
            || daemon::request("status")
                .ok()
                .is_some_and(|status| status.config_revision == Some(1)),
            Duration::from_secs(5)
        ),
        "the daemon never applied the edited config"
    );
    let after = daemon::request("status").expect("status");
    assert_eq!(after.paste.as_deref(), Some("ydotool"));
    assert_eq!(after.config_error, None);
}

#[test]
fn a_config_that_does_not_parse_keeps_the_last_good_one() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::start(
        "broken",
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("hello from the test"),
        &[],
    );

    fixture.write_config("paste = \"not a method\"\ntones = false\nhistory = false\n");

    assert!(
        wait_until(
            || daemon::request("status")
                .ok()
                .is_some_and(|status| status.config_error.is_some()),
            Duration::from_secs(5)
        ),
        "the daemon never reported the broken config"
    );
    let status = daemon::request("status").expect("status");
    assert_eq!(
        status.paste.as_deref(),
        Some("none"),
        "the last good config stopped being in force"
    );
    assert_eq!(status.config_revision, Some(0));

    // And a fixed file still lands, so the error is not sticky.
    fixture.write_config("paste = \"wtype\"\ntones = false\nhistory = false\n");
    assert!(
        wait_until(
            || daemon::request("status")
                .ok()
                .is_some_and(|status| status.config_revision == Some(1)),
            Duration::from_secs(5)
        ),
        "a corrected config was never applied"
    );
    let fixed = daemon::request("status").expect("status");
    assert_eq!(fixed.paste.as_deref(), Some("wtype"));
    assert_eq!(fixed.config_error, None);
}

#[test]
fn a_panicking_decode_is_reported_and_the_next_press_still_records() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let _fixture = Fixture::start(
        "panic",
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("hello from the test"),
        &[("HIPPO_DICTATE_FAKE_DECODE", "panic")],
    );

    daemon::request("toggle").expect("toggle on");
    assert!(
        wait_until(
            || daemon::request("status")
                .ok()
                .is_some_and(|status| status.phase.as_deref() == Some("recording")),
            Duration::from_secs(5)
        ),
        "the daemon never started recording"
    );
    std::thread::sleep(Duration::from_millis(150));
    daemon::request("toggle").expect("toggle off");

    assert!(
        wait_until(
            || daemon::request("status").ok().is_some_and(|status| {
                status.phase.as_deref() == Some("idle") && status.last_error.is_some()
            }),
            Duration::from_secs(15)
        ),
        "the daemon never reported the panic, or never left the decoding phase"
    );
    let reported = daemon::request("status")
        .expect("status")
        .last_error
        .expect("a reason");
    assert!(
        reported.contains("panicked"),
        "the reason does not say what happened: {reported}"
    );

    // The point of the supervision: the decoder thread outlives the panic.
    let text = deliver_once();
    assert_eq!(text.as_deref(), Some("hello from the test"));
    assert_eq!(
        daemon::request("status").expect("status").last_error,
        None,
        "the failure outlived the transcript that followed it"
    );
}

#[test]
fn a_missing_model_is_reported_at_startup() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    // A directory that exists and holds nothing: `resolve_dir` honours the
    // override, so the daemon starts and finds no model in it.
    let holder = Temp::new("no-model-dir");
    let model_dir = holder.0.join("empty-model");
    std::fs::create_dir_all(&model_dir).expect("create the empty model directory");

    let _fixture = Fixture::start(
        "no-model",
        "paste = \"none\"\ntones = false\n",
        None,
        &[(
            "HIPPO_DICTATE_MODEL_DIR",
            model_dir.to_str().expect("utf-8"),
        )],
    );

    let reported = daemon::request("status")
        .expect("status")
        .last_error
        .expect("the daemon started without saying the model was missing");
    assert!(
        reported.contains("no usable model"),
        "unexpected reason: {reported}"
    );
    assert!(
        reported.contains("model download"),
        "the reason does not say what to do about it: {reported}"
    );
}

#[test]
fn streaming_commits_a_phrase_while_the_key_is_still_down() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let _fixture = Fixture::start_with(
        "stream",
        "paste = \"none\"\ntones = false\nhistory = false\nstream_seconds = 0.5\n",
        Some("hello from the test"),
        &[],
        // Tone, then a pause: the pause is what a commit waits for, so this lands
        // before the second press.
        &tone_then_silence(0.4, 0.4),
    );

    daemon::toggle_stream().expect("stream on");
    assert!(
        wait_until(
            || daemon::request("status").ok().is_some_and(|status| {
                status.streaming == Some(true) && status.phase.as_deref() == Some("recording")
            }),
            Duration::from_secs(5)
        ),
        "the daemon never started a streaming capture"
    );

    assert!(
        wait_until(
            || daemon::request("status")
                .ok()
                .is_some_and(|status| status.text.is_some()),
            Duration::from_secs(5)
        ),
        "nothing was committed while the key was still down"
    );

    daemon::toggle_stream().expect("stream off");
    assert!(
        wait_until(
            || daemon::request("status").ok().is_some_and(|status| {
                status.phase.as_deref() == Some("idle") && status.streaming == Some(false)
            }),
            Duration::from_secs(10)
        ),
        "the streaming session never ended"
    );

    let status = daemon::request("status").expect("status");
    assert_eq!(status.text.as_deref(), Some("hello from the test"));
    assert_eq!(status.last_error, None);
}

#[test]
fn a_licence_is_reported_and_never_blocks_dictation() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    // The licence lives outside the fixture's own directory, because `Temp::new`
    // clears the directory it is given.
    let holder = Temp::new("licence-file");
    let licence = holder.0.join("licence");
    std::fs::write(&licence, "subject = \"nobody\"\ntier = \"pro\"\n---\n00\n")
        .expect("write the licence");

    let _fixture = Fixture::start(
        "licence",
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("hello from the test"),
        &[("HIPPO_DICTATE_LICENCE", licence.to_str().expect("utf-8"))],
    );

    let status = daemon::request("status").expect("status");
    assert!(
        status.tier.is_some(),
        "the daemon did not report a tier at all"
    );
    if status.tier.as_deref() == Some("free") {
        let note = status
            .licence
            .expect("a free tier with a licence file and no reason");
        assert!(
            note.to_lowercase().contains("licence"),
            "the reason does not mention the licence: {note}"
        );
    }

    // Nothing about licensing may stand between the user and their transcript.
    let text = deliver_once();
    assert_eq!(text.as_deref(), Some("hello from the test"));
}

#[test]
fn a_transcript_never_reaches_the_log() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::start(
        "redaction",
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("swordfish is the password"),
        &[],
    );

    let text = deliver_once();
    assert_eq!(text.as_deref(), Some("swordfish is the password"));

    let log = fixture.log();
    assert!(
        log.contains("delivered"),
        "the log says nothing about the delivery: {log}"
    );
    assert!(
        !log.contains("swordfish"),
        "the transcript reached the log: {log}"
    );

    // The delivery spans are what a tuning change is argued with.
    let timings = daemon::request("status")
        .expect("status")
        .timings
        .expect("no timings for the delivery");
    assert!(timings.total_ms > 0.0, "the total span is zero");
    assert!(timings.decode_ms > 0.0, "the decode span is zero");
    assert_eq!(
        timings.copy_ms, None,
        "a fake delivery reported a clipboard span"
    );
}

#[test]
fn a_client_from_another_build_is_refused_and_told_why() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let _fixture = Fixture::start(
        "protocol",
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("hello from the test"),
        &[],
    );

    // Raw, because `daemon::request` always sends this build's version: the point
    // is a client from a build that speaks something else.
    let mut stream = hippo_dictate::transport::connect().expect("connect");
    let request = serde_json::json!({ "cmd": "status", "protocol": 99 });
    serde_json::to_writer(&mut stream, &request).expect("write request");
    stream.write_all(b"\n").expect("newline");
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("read the reply");
    let reply: hippo_dictate::daemon::Reply = serde_json::from_str(&line).expect("parse the reply");

    assert!(!reply.ok, "a mismatched protocol was served");
    assert_eq!(
        reply.protocol,
        Some(1),
        "the daemon did not say its version"
    );
    let reason = reply.error.expect("no reason given");
    assert!(reason.contains("protocol"), "{reason}");
    assert!(
        reply.phase.is_none(),
        "the refused command was executed anyway"
    );

    // And the daemon is still usable by a client that speaks its version.
    assert_eq!(
        daemon::request("status").expect("status").phase.as_deref(),
        Some("idle")
    );
}

#[test]
fn a_filler_laden_transcript_arrives_clean() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let _fixture = Fixture::start(
        "cleanup",
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("um, mail sean at sign example dot com"),
        &[],
    );

    let text = deliver_once();
    assert_eq!(
        text.as_deref(),
        Some("mail sean@example.com"),
        "the cleanup pass did not run on the delivered text"
    );
}

#[test]
fn a_spoken_command_discards_the_transcript() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let _fixture = Fixture::start(
        "command",
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("scratch that"),
        &[],
    );

    daemon::request("toggle").expect("toggle on");
    assert!(
        wait_until(
            || daemon::request("status")
                .ok()
                .is_some_and(|status| status.phase.as_deref() == Some("recording")),
            Duration::from_secs(5)
        ),
        "the daemon never started recording"
    );
    std::thread::sleep(Duration::from_millis(150));
    daemon::request("toggle").expect("toggle off");

    assert!(
        wait_until(
            || daemon::request("status")
                .ok()
                .is_some_and(|status| status.phase.as_deref() == Some("idle")),
            Duration::from_secs(15)
        ),
        "the daemon never finished the command"
    );
    assert_eq!(
        daemon::request("status").expect("status").text,
        None,
        "a discarded utterance was delivered anyway"
    );
}

#[test]
fn a_spoken_quit_stops_the_daemon() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let _fixture = Fixture::start(
        "quit-command",
        "paste = \"none\"\ntones = false\nhistory = false\n",
        Some("quit hippo"),
        &[],
    );

    daemon::request("toggle").expect("toggle on");
    std::thread::sleep(Duration::from_millis(200));
    daemon::request("toggle").expect("toggle off");

    assert!(
        wait_until(
            || daemon::request("status")
                .err()
                .is_some_and(|error| hippo_dictate::transport::is_unreachable(&error)),
            Duration::from_secs(15)
        ),
        "the daemon did not stop on a spoken quit"
    );
}

#[test]
fn a_terminal_never_receives_a_newline() {
    let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    let _fixture = Fixture::start(
        "terminal-flatten",
        // Cleanup off on purpose: the tokenising pass happens to turn a newline
        // into a space, so this test only proves the flatten rule if it is not
        // there to hide the newline.
        "paste = \"none\"\ntones = false\nhistory = false\ncleanup = false\n",
        // The decoder can return a line break: a snippet with one, or a spoken
        // "new paragraph". Pasted into a shell, that is a command boundary.
        Some("hello\nworld"),
        &[("HIPPO_DICTATE_FAKE_FOCUS", "foot:1")],
    );

    let text = deliver_once();
    assert_eq!(text.as_deref(), Some("hello world"));
}
