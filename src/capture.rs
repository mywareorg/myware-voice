//! Microphone capture.
//!
//! A capture stream is opened per recording and closed when it stops. On Linux
//! `cpal` goes through ALSA (`pipewire-alsa` when PipeWire is in use), and an
//! ALSA stream that outlives a suspend/resume cycle silently stops delivering
//! frames — reopening per capture is the cheap way to never hold a dead stream.
//!
//! Samples are collected at the device's own rate and interleaving, then reduced
//! to mono and resampled by [`crate::audio`] before decoding.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

/// A running capture. Dropping it stops the device.
pub struct Recorder {
    stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    channels: usize,
    device: String,
    dropped: Arc<Mutex<Option<String>>>,
}

impl Recorder {
    /// Open the named device, or the host default when `device` is `None`.
    pub fn start(device: Option<&str>) -> Result<Self> {
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
            .with_context(|| format!("query {name}"))?;
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
        .with_context(|| format!("open {name}"))?;
        stream.play().context("start capture")?;

        Ok(Self {
            stream,
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

    /// Stop and hand over the interleaved samples with their rate.
    pub fn finish(self) -> (Vec<f32>, u32, usize) {
        let buffer = self
            .buffer
            .lock()
            .map(|samples| samples.clone())
            .unwrap_or_default();
        drop(self.stream);
        (buffer, self.rate, self.channels)
    }

    /// Stop and hand over mono samples already reduced from the device layout.
    pub fn finish_mono(self) -> (Vec<f32>, u32) {
        let (interleaved, rate, channels) = self.finish();
        (crate::audio::to_mono(&interleaved, channels), rate)
    }
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
    Err(anyhow!(
        "no input device matching {wanted:?}; run `hippo-voice doctor` to list them"
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
