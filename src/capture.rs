//! Microphone capture.
//!
//! A capture stream is opened per recording and closed when it stops. On Linux
//! `cpal` goes through ALSA (`pipewire-alsa` when PipeWire is in use), and an
//! ALSA stream that outlives a suspend/resume cycle silently stops delivering
//! frames — reopening per capture is the cheap way to never hold a dead stream.
//!
//! Samples are collected at the device's own rate and interleaving, then reduced
//! to mono and resampled by [`crate::audio`] before decoding.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

/// A running capture. Dropping it stops the device.
pub struct Recorder {
    /// `None` when replaying a file instead of holding a device.
    backing: Option<cpal::Stream>,
    buffer: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    channels: usize,
    device: String,
    dropped: Arc<Mutex<Option<String>>>,
}

impl Recorder {
    /// Open the named device, or the host default when `device` is `None`.
    ///
    /// `HIPPO_VOICE_FAKE_AUDIO` short-circuits this and replays a WAV instead,
    /// so the daemon's state machine can be tested with no microphone. It is an
    /// environment variable rather than a flag because a spawned daemon inherits
    /// the environment.
    pub fn start(device: Option<&str>) -> Result<Self> {
        if let Some(path) = fake_audio_path() {
            let (samples, rate) = crate::audio::read_wav(&path)
                .with_context(|| format!("fake audio {}", path.display()))?;
            if samples.is_empty() {
                return Err(anyhow!("fake audio {} is empty", path.display()));
            }
            let seconds = samples.len() as f32 / rate.max(1) as f32;
            // A test can ask for a device that dies immediately, which is how the
            // salvage path is exercised without unplugging anything.
            let dropped = if std::env::var_os("HIPPO_VOICE_FAKE_AUDIO_FAILS").is_some() {
                Some("simulated device loss".to_string())
            } else {
                None
            };
            return Ok(Self {
                backing: None,
                buffer: Arc::new(Mutex::new(samples)),
                rate,
                channels: 1,
                device: format!("fake audio ({seconds:.2}s)"),
                dropped: Arc::new(Mutex::new(dropped)),
            });
        }

        let host = cpal::default_host();
        let device = match device {
            Some(wanted) => find_input(&host, wanted)?,
            None => host
                .default_input_device()
                .ok_or_else(|| anyhow!("no default input device"))?,
        };
        let name = device.name().unwrap_or_else(|_| "input".to_string());

        let supported = device
            .default_input_config()
            .map_err(|error| explain("open the microphone", anyhow::Error::new(error)))?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let rate = config.sample_rate.0;
        let channels = config.channels as usize;

        let buffer = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 30)));
        let dropped = Arc::new(Mutex::new(None::<String>));

        let stream = build(
            &device,
            &config,
            sample_format,
            Arc::clone(&buffer),
            Arc::clone(&dropped),
        )
        .map_err(|error| explain(&format!("open {name}"), error))?;
        stream
            .play()
            .map_err(|error| explain("start capture", anyhow::Error::new(error)))?;

        Ok(Self {
            backing: Some(stream),
            buffer,
            rate,
            channels,
            device: name,
            dropped,
        })
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Seconds captured so far, from the frame count rather than a clock.
    pub fn seconds(&self) -> f32 {
        let frames = self.buffer.lock().map(|b| b.len()).unwrap_or(0) / self.channels.max(1);
        frames as f32 / self.rate as f32
    }

    /// A device error reported by the audio callback, if any.
    pub fn failure(&self) -> Option<String> {
        self.dropped.lock().ok().and_then(|slot| slot.clone())
    }

    /// Normalised magnitude for each logarithmically spaced frequency band.
    ///
    /// A 4096-sample Hann-windowed DFT at each band centre is enough for a
    /// 10 Hz meter and costs far less than pulling in an FFT crate for 100
    /// columns. The result is in 0..1 with a -60 dB floor, which is what the
    /// overlay draws; smoothing belongs to the display so a new capture starts
    /// from zero.
    pub fn bands(&self, count: usize) -> Vec<f32> {
        let Ok(samples) = self.buffer.lock() else {
            return Vec::new();
        };
        let channels = self.channels.max(1);
        let frames = samples.len() / channels;
        let window = frames.min(4096);
        if count == 0 || window < 64 {
            return Vec::new();
        }

        // Average the channels down to one waveform, same as the recognizer's
        // view, but keep only the most recent window.
        let from = (frames - window) * channels;
        let mono: Vec<f32> = (0..window)
            .map(|index| {
                let base = from + index * channels;
                samples[base..base + channels].iter().sum::<f32>() / channels as f32
            })
            .collect();
        let hann: Vec<f32> = (0..window)
            .map(|index| {
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * index as f32 / (window - 1) as f32).cos()
            })
            .collect();

        let rate = self.rate.max(1) as f32;
        // 30 Hz shifts the window below the old 50 Hz floor while the 4096
        // sample window gives ~12 Hz bins at 48 kHz, so the first columns have
        // something real to draw.
        let low = 30.0f32.min(rate * 0.05);
        let high = (rate * 0.5).min(8000.0).max(low * 2.0);
        let norm = window as f32 * 0.25;

        (0..count)
            .map(|band| {
                let position = band as f32 / (count.max(2) - 1) as f32;
                let frequency = low * (high / low).powf(position);
                let omega = 2.0 * std::f32::consts::PI * frequency / rate;
                let (step_sin, step_cos) = omega.sin_cos();
                let (mut sin_phase, mut cos_phase) = (0.0f32, 1.0f32);
                let (mut real, mut imaginary) = (0.0f32, 0.0f32);
                for (index, sample) in mono.iter().enumerate() {
                    let weighted = sample * hann[index];
                    real += weighted * cos_phase;
                    imaginary += weighted * sin_phase;
                    // Advance by omega without a trig call per sample. Over
                    // 4096 steps the drift is far below the display resolution.
                    let next_sin = sin_phase * step_cos + cos_phase * step_sin;
                    let next_cos = cos_phase * step_cos - sin_phase * step_sin;
                    sin_phase = next_sin;
                    cos_phase = next_cos;
                }
                let magnitude = (real * real + imaginary * imaginary).sqrt() / norm;
                let db = 20.0 * magnitude.max(1e-6).log10();
                ((db + 60.0) / 60.0).clamp(0.0, 1.0)
            })
            .collect()
    }

    /// RMS of the most recent ~2048 frames, for the listening popup.
    ///
    /// Deliberately cheap: the buffer is already there, and the display only
    /// needs a number every 50 ms. It reads the tail rather than resampling, so
    /// the meter tracks the device's own signal, not the recognizer's 16 kHz
    /// view of it.
    pub fn level(&self) -> f32 {
        let Ok(samples) = self.buffer.lock() else {
            return 0.0;
        };
        let channels = self.channels.max(1);
        let frames = samples.len() / channels;
        let take = frames.min(2048);
        if take == 0 {
            return 0.0;
        }
        let from = (frames - take) * channels;
        let window = &samples[from..];
        let energy: f32 = window.iter().map(|sample| sample * sample).sum();
        (energy / window.len().max(1) as f32).sqrt()
    }

    /// Stop and hand over the interleaved samples with their rate.
    pub fn finish(self) -> (Vec<f32>, u32, usize) {
        let buffer = self
            .buffer
            .lock()
            .map(|samples| samples.clone())
            .unwrap_or_default();
        drop(self.backing);
        (buffer, self.rate, self.channels)
    }

    /// Stop and hand over mono samples already reduced from the device layout.
    pub fn finish_mono(self) -> (Vec<f32>, u32) {
        let (interleaved, rate, channels) = self.finish();
        (crate::audio::to_mono(&interleaved, channels), rate)
    }

    /// A copy of everything captured so far, reduced to mono, without stopping.
    ///
    /// Streaming commits phrases while the device keeps running, so it cannot
    /// use `finish_mono`. The copy is deliberate: resampling and decoding happen
    /// on the daemon thread while the audio callback keeps appending.
    pub fn snapshot_mono(&self) -> (Vec<f32>, u32) {
        let interleaved = self
            .buffer
            .lock()
            .map(|samples| samples.clone())
            .unwrap_or_default();
        (
            crate::audio::to_mono(&interleaved, self.channels),
            self.rate,
        )
    }
}

/// Turn a device error into something the user can act on.
///
/// The two that matter are a blocked microphone (Windows 10+ privacy setting, or
/// a denied permission) and a device that is busy or gone. Everything else keeps
/// the original text, which is all a bug report needs.
fn explain(what: &str, error: anyhow::Error) -> anyhow::Error {
    let text = format!("{error:#}").to_lowercase();

    let denied = text.contains("access is denied")
        || text.contains("permission denied")
        || text.contains("0x80070005")
        || text.contains("e_accessdenied");
    if denied {
        return if cfg!(windows) {
            anyhow!(
                "microphone access is off: Settings > Privacy & security > Microphone,                  then allow desktop apps ({what})"
            )
        } else {
            anyhow!("microphone access was denied ({what})")
        };
    }

    let busy = text.contains("device or resource busy")
        || text.contains("0x8007001f")
        || text.contains("device in use");
    if busy {
        return anyhow!("the microphone is being used by another program ({what})");
    }

    let gone = text.contains("no such file or directory")
        || text.contains("0x80070490")
        || text.contains("no default input device")
        || text.contains("element not found");
    if gone {
        return anyhow!("no microphone is connected ({what})");
    }

    anyhow!("{what}: {error}")
}

/// A WAV to replay instead of the microphone. Tests only.
fn fake_audio_path() -> Option<PathBuf> {
    std::env::var_os("HIPPO_VOICE_FAKE_AUDIO").map(PathBuf::from)
}

fn find_input(host: &cpal::Host, wanted: &str) -> Result<cpal::Device> {
    let devices = host.input_devices().context("enumerate input devices")?;
    for device in devices {
        if let Ok(name) = device.name() {
            if name.to_lowercase().contains(&wanted.to_lowercase()) {
                return Ok(device);
            }
        }
    }
    let available = input_devices().unwrap_or_default();
    Err(anyhow!(
        "no input device matching {wanted:?}; available: {}",
        if available.is_empty() {
            "none".to_string()
        } else {
            available.join(", ")
        }
    ))
}

/// Every input device the host can see.
pub fn input_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let names = host
        .input_devices()
        .context("enumerate input devices")?
        .filter_map(|device| device.name().ok())
        .collect();
    Ok(names)
}

macro_rules! build_stream {
    ($device:expr, $config:expr, $ty:ty, $push:expr, $err:expr) => {{
        let push = $push;
        $device.build_input_stream(
            $config,
            move |data: &[$ty], _info: &cpal::InputCallbackInfo| push(data),
            $err,
            None,
        )
    }};
}

fn build(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: SampleFormat,
    buffer: Arc<Mutex<Vec<f32>>>,
    dropped: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream> {
    let err_dropped = Arc::clone(&dropped);
    let err_callback = move |error: cpal::StreamError| {
        if let Ok(mut slot) = err_dropped.lock() {
            *slot = Some(error.to_string());
        }
        eprintln!("audio: {error}");
    };

    let stream = match format {
        SampleFormat::F32 => {
            let sink = Arc::clone(&buffer);
            build_stream!(
                device,
                config,
                f32,
                move |data: &[f32]| {
                    if let Ok(mut samples) = sink.lock() {
                        samples.extend_from_slice(data);
                    }
                },
                err_callback
            )?
        }
        SampleFormat::I16 => {
            let sink = Arc::clone(&buffer);
            build_stream!(
                device,
                config,
                i16,
                move |data: &[i16]| {
                    if let Ok(mut samples) = sink.lock() {
                        samples.extend(data.iter().map(|s| *s as f32 / 32768.0));
                    }
                },
                err_callback
            )?
        }
        SampleFormat::U16 => {
            let sink = Arc::clone(&buffer);
            build_stream!(
                device,
                config,
                u16,
                move |data: &[u16]| {
                    if let Ok(mut samples) = sink.lock() {
                        samples.extend(data.iter().map(|s| (*s as f32 - 32768.0) / 32768.0));
                    }
                },
                err_callback
            )?
        }
        other => return Err(anyhow!("unsupported sample format {other:?}")),
    };
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::explain;

    #[test]
    fn a_blocked_microphone_is_explained_in_words() {
        let error = explain(
            "open the microphone",
            anyhow::anyhow!("Access is denied. (0x80070005)"),
        );
        let text = error.to_string();
        assert!(text.contains("microphone access"), "{text}");
        assert!(
            !text.contains("0x80070005"),
            "the raw code should not be the whole message: {text}"
        );
    }

    #[test]
    fn a_busy_or_absent_device_is_named() {
        let busy = explain(
            "open the microphone",
            anyhow::anyhow!("Device or resource busy (os error 16)"),
        );
        assert!(
            busy.to_string().contains("used by another program"),
            "{busy}"
        );

        let gone = explain(
            "open the microphone",
            anyhow::anyhow!("No such file or directory (os error 2)"),
        );
        assert!(
            gone.to_string().contains("no microphone is connected"),
            "{gone}"
        );
    }

    #[test]
    fn anything_else_keeps_its_original_text() {
        let odd = explain(
            "open the microphone",
            anyhow::anyhow!("something unexpected"),
        );
        assert_eq!(odd.to_string(), "open the microphone: something unexpected");
    }
}
