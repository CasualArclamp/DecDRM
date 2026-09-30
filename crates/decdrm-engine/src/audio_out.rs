//! Audio outputs of the engine: live playback (drift-compensated) and recording, plus
//! the spectrum of the decoded audio for user interfaces.

use crate::snapshot::AudioSpectrum;
use anyhow::{Context, Result};
use decdrm_core::Cplx;
use decdrm_core::dsp::fft::Fft;
use decdrm_io::{AudioFormat, AudioPlayer, Container, Encoding, FileWriter, PlayerOptions};
use std::path::PathBuf;
use std::time::Duration;

/// FFT length of the audio spectrum: 11.7 Hz bins at 24 kHz, 23.4 Hz at 48 kHz.
pub const AUDIO_FFT_LEN: usize = 2048;
/// Time constant of the audio spectrum's exponential average, seconds.
pub const AUDIO_AVERAGE_S: f64 = 0.3;

/// Where decoded audio goes.
pub struct AudioOut {
    player: Option<AudioPlayer>,
    /// Pace pushes to the sound card (file playback) instead of dropping excess
    /// audio (live reception, where the drift loop keeps the queue on target).
    blocking: bool,
    recorder: Option<Recorder>,
    analyser: AudioAnalyser,
}

struct Recorder {
    path: PathBuf,
    writer: Option<FileWriter>,
    format: Option<AudioFormat>,
}

impl AudioOut {
    /// `play`: open the output device (`device` = name or `None` for default).
    /// `blocking`: file playback (pace the decoder to the sound card).
    pub fn new(play: bool, device: Option<String>, blocking: bool, record: Option<PathBuf>) -> Result<Self> {
        let player = if play {
            let opts = PlayerOptions {
                device,
                // Avoid devices whose mix format is 192 kHz.
                sample_rate: Some(48_000),
                drift_compensation: !blocking,
                ..PlayerOptions::default()
            };
            Some(AudioPlayer::open(opts).context("opening audio output")?)
        } else {
            None
        };
        Ok(Self {
            player,
            blocking,
            recorder: record.map(|path| Recorder { path, writer: None, format: None }),
            analyser: AudioAnalyser::new(),
        })
    }

    pub fn is_playing(&self) -> bool {
        self.player.is_some()
    }

    /// Playback volume (linear gain; the recording and the spectrum are unaffected).
    pub fn set_volume(&mut self, gain: f32) {
        if let Some(p) = self.player.as_ref() {
            p.set_volume(gain);
        }
    }

    /// Queue decoded audio (interleaved, `channels` = 1 or 2).
    pub fn push(&mut self, samples: &[f32], sample_rate: u32, channels: usize) -> Result<()> {
        self.analyser.push(samples, sample_rate, channels);
        if let Some(p) = self.player.as_mut() {
            if self.blocking {
                p.push_blocking(samples, sample_rate, channels)?;
            } else {
                p.push(samples, sample_rate, channels)?;
            }
        }
        if let Some(r) = self.recorder.as_mut() {
            let fmt = AudioFormat::new(sample_rate, channels);
            if r.format != Some(fmt) {
                // A new format (e.g. after a service change) starts a new file.
                if let Some(w) = r.writer.take() {
                    w.finalize()?;
                }
                let path = if r.format.is_none() {
                    r.path.clone()
                } else {
                    let stem = r.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let ext = r.path.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "wav".into());
                    r.path.with_file_name(format!("{stem}-{}.{ext}", chrono_like_stamp()))
                };
                let container = Container::from_path(&path).unwrap_or(Container::Wav);
                r.writer = Some(FileWriter::create(&path, fmt, container, Encoding::Int16)?);
                r.format = Some(fmt);
            }
            if let Some(w) = r.writer.as_mut() {
                w.write(samples)?;
            }
        }
        Ok(())
    }

    /// Queue depth and drift correction for the status display.
    pub fn status(&self) -> Option<(Duration, f64)> {
        self.player.as_ref().map(|p| (p.buffered(), p.ppm()))
    }

    /// The smoothed spectrum of the audio pushed so far.
    pub fn spectrum(&self) -> AudioSpectrum {
        self.analyser.spectrum()
    }

    /// Finish: let queued audio play out and close the recording.
    pub fn finish(&mut self) -> Result<()> {
        if let Some(p) = self.player.as_mut() {
            p.drain(Duration::from_secs(3));
        }
        if let Some(r) = self.recorder.as_mut()
            && let Some(w) = r.writer.take()
        {
            w.finalize()?;
        }
        Ok(())
    }
}

/// Seconds since the Unix epoch, for unique file names without a date crate.
fn chrono_like_stamp() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Smoothed power spectrum of the decoded audio, for display.
///
/// The channels are averaged to one, cut into consecutive blocks of [`AUDIO_FFT_LEN`]
/// samples and Hann-windowed. Blocks do not overlap, which keeps the cost at about 23
/// FFTs per second of 48 kHz audio (a few hundred microseconds of CPU); for a display
/// averaged over several blocks the samples the window tapers off do not matter. The
/// power per bin is scaled so that a full-scale sine reads 0 dB, and averaged
/// exponentially with a time constant of [`AUDIO_AVERAGE_S`] whatever the sample rate
/// (the weight of the old average per block is e^(−T/τ) for a block of T seconds). A
/// change of sample rate starts afresh.
pub struct AudioAnalyser {
    fft: Fft,
    window: Vec<f64>,
    /// (2 / Σw)²: a sine of amplitude A then reads A² in its bin.
    scale: f64,
    /// Mono samples of the block being collected.
    block: Vec<f64>,
    work: Vec<Cplx>,
    /// Averaged power of bins 0 … N/2 (linear).
    avg: Vec<f64>,
    sample_rate: u32,
    channels: usize,
    blocks: u64,
}

impl Default for AudioAnalyser {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioAnalyser {
    pub fn new() -> Self {
        let n = AUDIO_FFT_LEN;
        // Periodic Hann window (the FFT's frame is one period).
        let window: Vec<f64> = (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()).collect();
        let sum: f64 = window.iter().sum();
        Self {
            fft: Fft::new(n),
            window,
            scale: (2.0 / sum).powi(2),
            block: Vec::with_capacity(n),
            work: vec![Cplx::new(0.0, 0.0); n],
            avg: vec![0.0; n / 2 + 1],
            sample_rate: 0,
            channels: 0,
            blocks: 0,
        }
    }

    /// Feed interleaved samples (`channels` per frame; a partial frame at the end is
    /// ignored).
    pub fn push(&mut self, samples: &[f32], sample_rate: u32, channels: usize) {
        let ch = channels.max(1);
        if sample_rate != self.sample_rate {
            self.block.clear();
            self.avg.iter_mut().for_each(|a| *a = 0.0);
            self.blocks = 0;
            self.sample_rate = sample_rate;
        }
        self.channels = ch;
        for frame in samples.chunks_exact(ch) {
            let sum: f64 = frame.iter().map(|&s| f64::from(s)).sum();
            self.block.push(sum / ch as f64);
            if self.block.len() == AUDIO_FFT_LEN {
                self.analyse();
                self.block.clear();
            }
        }
    }

    fn analyse(&mut self) {
        for ((out, &x), &w) in self.work.iter_mut().zip(&self.block).zip(&self.window) {
            *out = Cplx::new(x * w, 0.0);
        }
        self.fft.forward(&mut self.work);
        let lambda = if self.blocks == 0 {
            0.0
        } else {
            let block_s = AUDIO_FFT_LEN as f64 / f64::from(self.sample_rate.max(1));
            (-block_s / AUDIO_AVERAGE_S).exp()
        };
        // `avg` holds bins 0 … N/2 only (a real signal's spectrum is symmetric), so the
        // zip stops there.
        for (a, x) in self.avg.iter_mut().zip(&self.work) {
            *a = lambda * *a + (1.0 - lambda) * x.norm_sqr() * self.scale;
        }
        self.blocks += 1;
    }

    /// The averaged spectrum; empty before the first complete block.
    pub fn spectrum(&self) -> AudioSpectrum {
        if self.blocks == 0 {
            return AudioSpectrum::default();
        }
        AudioSpectrum {
            db: self.avg.iter().map(|p| 10.0 * p.max(1e-20).log10()).collect(),
            bin_hz: f64::from(self.sample_rate) / AUDIO_FFT_LEN as f64,
            sample_rate: self.sample_rate,
            channels: u8::try_from(self.channels).unwrap_or(u8::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const N: usize = AUDIO_FFT_LEN;

    fn argmax(v: &[f64]) -> usize {
        v.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap()
    }

    /// `frames` frames of a sine at bin `k` with amplitude `a`, the same on every channel.
    fn tone(k: usize, a: f64, frames: usize, channels: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let v = (a * (2.0 * PI * k as f64 * n as f64 / N as f64).sin()) as f32;
                std::iter::repeat_n(v, channels)
            })
            .collect()
    }

    #[test]
    fn nothing_before_a_complete_block() {
        let mut a = AudioAnalyser::new();
        assert_eq!(a.spectrum(), AudioSpectrum::default());
        a.push(&tone(10, 0.5, N - 1, 1), 48_000, 1);
        assert!(a.spectrum().db.is_empty());
        a.push(&tone(10, 0.5, 1, 1), 48_000, 1);
        let s = a.spectrum();
        assert_eq!(s.db.len(), N / 2 + 1);
        assert_eq!((s.sample_rate, s.channels), (48_000, 1));
        assert!((s.bin_hz - 48_000.0 / N as f64).abs() < 1e-12);
    }

    #[test]
    fn full_scale_sine_reads_zero_db() {
        let k = 100;
        let mut a = AudioAnalyser::new();
        a.push(&tone(k, 1.0, 4 * N, 2), 24_000, 2);
        let s = a.spectrum();
        assert_eq!(argmax(&s.db), k);
        assert!(s.db[k].abs() < 0.01, "{} dB", s.db[k]);
        // Hann sidelobes and leakage fall off quickly: far bins are very low.
        assert!(s.db[k + 50] < -100.0, "{} dB", s.db[k + 50]);
        assert_eq!(s.channels, 2);
        // Half the amplitude: −6 dB.
        let mut b = AudioAnalyser::new();
        b.push(&tone(k, 0.5, N, 1), 24_000, 1);
        assert!((b.spectrum().db[k] + 6.02).abs() < 0.01);
    }

    #[test]
    fn channels_are_averaged() {
        // Left and right in antiphase cancel out.
        let mut x = tone(40, 1.0, N, 2);
        for frame in x.as_chunks_mut::<2>().0 {
            frame[1] = -frame[0];
        }
        let mut a = AudioAnalyser::new();
        a.push(&x, 48_000, 2);
        assert!(a.spectrum().db.iter().all(|&d| d < -150.0));
    }

    #[test]
    fn average_follows_changes_and_a_new_rate_restarts() {
        let mut a = AudioAnalyser::new();
        a.push(&tone(64, 1.0, N, 1), 48_000, 1);
        // Silence: the tone decays with the time constant (one 42.7 ms block ≈ −0.62 dB).
        a.push(&vec![0.0; N], 48_000, 1);
        let one_block = a.spectrum().db[64];
        let expected = 10.0 * (-(N as f64 / 48_000.0) / AUDIO_AVERAGE_S).exp().log10();
        assert!((one_block - expected).abs() < 0.01, "{one_block} vs {expected}");
        // A different sample rate: the average starts again from the new audio.
        a.push(&tone(8, 0.5, N, 1), 12_000, 1);
        let s = a.spectrum();
        assert_eq!(s.sample_rate, 12_000);
        assert_eq!(argmax(&s.db), 8);
        assert!(s.db[64] < -100.0, "the old tone is gone");
    }
}
