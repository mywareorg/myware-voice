//! Audio in, and the 16 kHz mono the recognizer expects.
//!
//! The recognizer wants mono f32 at 16 kHz. A microphone gives whatever the
//! device runs at — usually 44.1 or 48 kHz, often stereo — so capture converts
//! here rather than asking the device for a rate it may not support.

use std::path::Path;

use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavReader};
use rubato::Resampler;

/// Rate Parakeet expects. Anything else is resampled before decoding.
pub const SAMPLE_RATE: u32 = 16000;

/// Loudness the normaliser aims for. A single global gain, not compression: the
/// model needs quiet input lifted to a workable level, and a constant gain cannot
/// pump between words.
const TARGET_RMS: f32 = 0.1;
const PEAK_CEILING: f32 = 0.891;
const FRAME_FLOOR: f32 = 1e-3;

/// Read a WAV file as mono f32 in its own sample rate.
pub fn read_wav(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader = WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
    let spec = reader.spec();

    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()
            .context("read f32 samples")?,
        (SampleFormat::Int, bits) => {
            let scale = 1.0 / (1i64 << (bits - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<std::result::Result<_, _>>()
                .context("read integer samples")?
        }
        (format, bits) => bail!("unsupported WAV format {format:?} at {bits} bits"),
    };

    let channels = spec.channels.max(1) as usize;
    Ok((to_mono(&interleaved, channels), spec.sample_rate))
}

/// Write mono f32 as 16-bit PCM.
pub fn write_wav(path: &Path, samples: &[f32], rate: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("create {}", path.display()))?;
    for sample in samples {
        writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Average interleaved channels down to one.
pub fn to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Resample mono f32 to `to` hertz.
pub fn resample(input: &[f32], from: u32, to: u32) -> Result<Vec<f32>> {
    if from == to || input.is_empty() {
        return Ok(input.to_vec());
    }

    // 4096 frames is ~93 ms at 44.1 kHz: large enough that FFT setup does not
    // dominate, small enough that a short utterance is still one block.
    let chunk = 4096;
    let mut resampler = rubato::FftFixedInOut::<f32>::new(from as usize, to as usize, chunk, 1)
        .context("build resampler")?;

    let mut output = Vec::with_capacity(input.len() * to as usize / from as usize + chunk);
    let mut block = vec![0.0f32; resampler.input_frames_next()];
    let mut pos = 0;
    while pos < input.len() {
        let need = resampler.input_frames_next();
        if block.len() < need {
            block.resize(need, 0.0);
        }
        block[..need].fill(0.0);
        let available = (input.len() - pos).min(need);
        block[..available].copy_from_slice(&input[pos..pos + available]);
        pos += available;

        let processed = resampler
            .process(&[&block[..need]], None)
            .context("resample")?;
        output.extend_from_slice(&processed[0]);
    }
    Ok(output)
}

/// Bring a recording to a usable level in place. Returns the gain applied.
pub fn normalize_loudness(samples: &mut [f32]) -> f32 {
    if samples.is_empty() {
        return 1.0;
    }

    let window = (SAMPLE_RATE as usize / 50).max(1); // 20 ms
    let mut peak = 0.0f32;
    let mut frames = 0usize;
    let mut sum_sq = 0.0f64;
    for chunk in samples.chunks(window) {
        let mut sq = 0.0f64;
        for sample in chunk {
            let value = *sample as f64;
            sq += value * value;
            peak = peak.max(sample.abs());
        }
        let rms = (sq / chunk.len() as f64).sqrt() as f32;
        if rms > FRAME_FLOOR {
            sum_sq += (rms as f64) * (rms as f64);
            frames += 1;
        }
    }

    if frames == 0 || peak <= f32::EPSILON {
        return 1.0;
    }
    let rms = (sum_sq / frames as f64).sqrt() as f32;
    if rms <= FRAME_FLOOR {
        return 1.0;
    }

    let mut gain = (TARGET_RMS / rms).clamp(0.5, 8.0);
    if peak * gain > PEAK_CEILING {
        gain = PEAK_CEILING / peak;
    }
    if (gain - 1.0).abs() < 0.01 {
        return 1.0;
    }
    for sample in samples.iter_mut() {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
    gain
}

/// Everything the recognizer wants from a file: mono, 16 kHz, levelled.
pub fn prepare(path: &Path) -> Result<(Vec<f32>, f32)> {
    let (samples, rate) = read_wav(path)?;
    let mut samples = resample(&samples, rate, SAMPLE_RATE)?;
    let gain = normalize_loudness(&mut samples);
    Ok((samples, gain))
}

/// Split at silences into pieces no longer than roughly `target` seconds.
///
/// Parakeet must not be handed a long clip: its accuracy collapses and its
/// memory grows, and the model card is explicit that fixed windows are not a
/// substitute. Each cut is placed in the quietest 20 ms frame within a window
/// around the target length, so a sentence is never divided mid-word unless the
/// speaker never pauses.
pub fn split_at_silence(samples: &[f32], target: f32) -> Vec<(usize, usize)> {
    let rate = SAMPLE_RATE as f32;
    let target_frames = (target * rate).max(1.0) as usize;
    let frame = (0.02 * rate) as usize; // 20 ms
    let min_frames = target_frames / 2;
    let max_frames = (target * 1.2 * rate) as usize;

    let mut spans = Vec::new();
    let mut start = 0usize;
    while start < samples.len() {
        let remaining = samples.len() - start;
        if remaining <= max_frames {
            spans.push((start, samples.len()));
            break;
        }

        let from = start + min_frames;
        let to = (start + max_frames).min(samples.len() - frame);
        if from >= to {
            spans.push((start, samples.len()));
            break;
        }

        // The quietest frame in the search window, measured as mean square.
        let mut best = (f32::MAX, from);
        let mut cursor = from;
        while cursor + frame <= to {
            let energy: f32 = samples[cursor..cursor + frame]
                .iter()
                .map(|sample| sample * sample)
                .sum::<f32>()
                / frame as f32;
            if energy < best.0 {
                best = (energy, cursor);
            }
            cursor += frame;
        }

        let cut = best.1 + frame / 2;
        if cut <= start {
            spans.push((start, samples.len()));
            break;
        }
        spans.push((start, cut));
        start = cut;
    }

    if spans.is_empty() {
        spans.push((0, samples.len()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loud tone with silent gaps, so the quietest frame is unambiguous.
    fn speech_like(seconds: f32) -> Vec<f32> {
        let rate = SAMPLE_RATE as usize;
        let total = (seconds * rate as f32) as usize;
        (0..total)
            .map(|index| {
                let in_gap = (index / (rate / 2)) % 4 == 3; // 0.5 s silence every 2 s
                if in_gap {
                    0.0
                } else {
                    ((index as f32 * 0.05).sin()) * 0.3
                }
            })
            .collect()
    }

    #[test]
    fn long_audio_is_cut_into_target_sized_pieces() {
        let samples = speech_like(60.0);
        let spans = split_at_silence(&samples, 20.0);
        assert!(spans.len() >= 3, "60 s at 20 s must not be one clip");
        for (start, end) in &spans {
            let seconds = (end - start) as f32 / SAMPLE_RATE as f32;
            assert!(seconds <= 20.0 * 1.2 + 0.01, "chunk too long: {seconds}");
        }
        assert_eq!(spans.first().unwrap().0, 0);
        assert_eq!(spans.last().unwrap().1, samples.len());
        // Contiguous: no samples dropped between chunks.
        for pair in spans.windows(2) {
            assert_eq!(pair[0].1, pair[1].0);
        }
    }

    #[test]
    fn short_audio_is_left_whole() {
        let samples = speech_like(5.0);
        let spans = split_at_silence(&samples, 20.0);
        assert_eq!(spans, vec![(0, samples.len())]);
    }

    #[test]
    fn a_cut_lands_in_a_silence_when_one_is_available() {
        let samples = speech_like(40.0);
        let spans = split_at_silence(&samples, 20.0);
        for (_, end) in spans.iter().take(spans.len() - 1) {
            let from = end.saturating_sub(SAMPLE_RATE as usize / 200);
            let to = (*end + SAMPLE_RATE as usize / 200).min(samples.len());
            let energy: f32 = samples[from..to].iter().map(|s| s * s).sum();
            assert!(energy < 1.0, "cut at {end} is inside speech");
        }
    }
}
