//! Audio outputs of the engine: live playback (drift-compensated) and recording, plus
//! the spectrum of the decoded audio for user interfaces. The RF monitor plays the
//! receiver's input instead of the decoded audio.

use crate::snapshot::{AudioSpectrum, RecordingStatus};
use anyhow::{Context, Result};
use decdrm_core::Cplx;
use decdrm_core::dsp::fft::Fft;
use decdrm_io::{AudioFormat, AudioPlayer, Container, Encoding, FileWriter, PlayerOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// FFT length of the audio spectrum: 11.7 Hz bins at 24 kHz, 23.4 Hz at 48 kHz.
pub const AUDIO_FFT_LEN: usize = 2048;
/// Time constant of the audio spectrum's exponential average, seconds.
pub const AUDIO_AVERAGE_S: f64 = 0.3;

/// Seconds of audio between two checkpoints of a recording ([`FileWriter::flush`]): a
/// crash or a power cut loses at most this much of a WAV recording.
pub const RECORDING_CHECKPOINT_S: f64 = 5.0;

/// Where decoded audio goes.
pub struct AudioOut {
    player: Option<AudioPlayer>,
    /// Pace pushes to the sound card (file playback) instead of dropping excess
    /// audio (live reception, where the drift loop keeps the queue on target).
    blocking: bool,
    /// The recording, kept after it ends (for its final state).
    recorder: Option<Recorder>,
    /// Format of the latest audio: a recording started now opens its file at once.
    format: Option<AudioFormat>,
    analyser: AudioAnalyser,
    /// RF monitor: the sound card plays the input ([`Self::push_monitor`]); the decoded
    /// audio is still recorded and analysed.
    monitor: bool,
}

/// A recording of the decoded audio as it is decoded: the service's sample rate and
/// channels, 16-bit, WAV (or FLAC, by the file name's extension). A WAV or FLAC file
/// has one format, so a change of format (another service, a reconfigured one)
/// carries on in a new file: `name-2.wav`, `name-3.wav`, … (never one that exists).
/// Signal losses are not filled in: the recording holds the audio that was decoded.
struct Recorder {
    /// The file asked for.
    path: PathBuf,
    writer: Option<FileWriter>,
    /// Frames of the current file at its last checkpoint.
    flushed: u64,
    /// Files opened so far, the current one last.
    files: Vec<PathBuf>,
    /// Seconds of audio in the files before the current one.
    done_s: f64,
    active: bool,
    error: Option<String>,
}

impl Recorder {
    fn new(path: PathBuf) -> Self {
        Self { path, writer: None, flushed: 0, files: Vec::new(), done_s: 0.0, active: true, error: None }
    }

    /// Write `samples` of format `fmt`, opening the first file, or the next one when
    /// the format has changed.
    fn write(&mut self, samples: &[f32], fmt: AudioFormat) -> Result<()> {
        if self.writer.as_ref().is_none_or(|w| w.format() != fmt) {
            self.open(fmt)?;
        }
        let Some(w) = self.writer.as_mut() else { return Ok(()) };
        w.write(samples).with_context(|| format!("writing {}", w.path().display()))?;
        if (w.frames_written() - self.flushed) as f64 >= RECORDING_CHECKPOINT_S * f64::from(fmt.sample_rate) {
            w.flush().with_context(|| format!("writing {}", w.path().display()))?;
            self.flushed = w.frames_written();
        }
        Ok(())
    }

    /// Complete the current file (if any) and open the next one for `fmt`.
    fn open(&mut self, fmt: AudioFormat) -> Result<()> {
        self.close()?;
        let path = if self.files.is_empty() { self.path.clone() } else { next_part(&self.path, self.files.len() + 1) };
        let container = Container::from_path(&path).unwrap_or(Container::Wav);
        let writer = FileWriter::create(&path, fmt, container, Encoding::Int16)
            .with_context(|| format!("cannot record to {}", path.display()))?;
        self.files.push(path);
        self.writer = Some(writer);
        self.flushed = 0;
        Ok(())
    }

    /// Complete the current file.
    fn close(&mut self) -> Result<()> {
        if let Some(w) = self.writer.take() {
            self.done_s += w.frames_written() as f64 / f64::from(w.format().sample_rate);
            let path = w.path().to_path_buf();
            w.finalize().with_context(|| format!("completing {}", path.display()))?;
        }
        Ok(())
    }

    /// End the recording: complete its file.
    fn stop(&mut self) -> Result<()> {
        self.active = false;
        let closed = self.close();
        if let Err(e) = &closed {
            self.error = Some(format!("{e:#}"));
        }
        closed
    }

    /// End the recording because of `e` (the file is completed as far as possible).
    fn fail(&mut self, e: &anyhow::Error) {
        self.active = false;
        self.error = Some(format!("{e:#}"));
        if let Some(w) = self.writer.take() {
            self.done_s += w.frames_written() as f64 / f64::from(w.format().sample_rate);
            // Dropping a writer completes it on a best-effort basis.
            drop(w);
        }
    }

    fn status(&self) -> RecordingStatus {
        let current = self.writer.as_ref().map_or(0.0, |w| w.frames_written() as f64 / f64::from(w.format().sample_rate));
        RecordingStatus {
            path: self.path.clone(),
            files: self.files.clone(),
            seconds: self.done_s + current,
            format: self.writer.as_ref().map(|w| (w.format().sample_rate, w.format().channels)),
            active: self.active,
            error: self.error.clone(),
        }
    }
}

/// The file for part `n` (2, 3, …) of the recording asked for as `path`: `name-n.ext`,
/// or the next number whose file does not exist yet.
fn next_part(path: &Path, n: usize) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "wav".into());
    (n..)
        .map(|k| path.with_file_name(format!("{stem}-{k}.{ext}")))
        .find(|p| !p.exists())
        .expect("a free file name")
}

impl AudioOut {
    /// `play`: open the output device (`device` = name or `None` for default).
    /// `blocking`: file playback (pace the decoder to the sound card).
    /// `record`: record the decoded audio to this file from the start (see
    /// [`Self::start_recording`]).
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
            recorder: record.map(Recorder::new),
            format: None,
            analyser: AudioAnalyser::new(),
            monitor: false,
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

    /// The RF monitor: play the receiver's input instead of the decoded audio (or the
    /// decoded audio again). The audio already queued on the sound card plays out first.
    pub fn set_monitor(&mut self, on: bool) {
        self.monitor = on;
    }

    /// Whether the RF monitor is on.
    pub fn monitoring(&self) -> bool {
        self.monitor
    }

    /// Queue audio on the sound card: paced to it for file playback, else dropping
    /// what would make the queue too deep.
    fn play(&mut self, samples: &[f32], sample_rate: u32, channels: usize) -> Result<()> {
        match self.player.as_mut() {
            Some(p) if self.blocking => p.push_blocking(samples, sample_rate, channels)?,
            // (The number of frames queued is of no interest here.)
            Some(p) => drop(p.push(samples, sample_rate, channels)?),
            None => {}
        }
        Ok(())
    }

    /// The RF monitor's sound: the receiver's input frames as they come in (48 kHz,
    /// interleaved; 2 channels for I/Q or a stereo input), played while the monitor is
    /// on.
    pub fn push_monitor(&mut self, frames: &[f32], channels: usize) -> Result<()> {
        if self.monitor { self.play(frames, decdrm_core::params::SAMPLE_RATE, channels) } else { Ok(()) }
    }

    /// Queue decoded audio (interleaved, `channels` = 1 or 2); while the RF monitor is
    /// on it is only recorded and analysed. A recording that cannot be written ends
    /// with the error, which is returned once; playback goes on.
    pub fn push(&mut self, samples: &[f32], sample_rate: u32, channels: usize) -> Result<()> {
        self.analyser.push(samples, sample_rate, channels);
        let fmt = AudioFormat::new(sample_rate, channels);
        self.format = Some(fmt);
        let played = if self.monitor { Ok(()) } else { self.play(samples, sample_rate, channels) };
        if let Some(r) = self.recorder.as_mut().filter(|r| r.active)
            && let Err(e) = r.write(samples, fmt)
        {
            r.fail(&e);
            return Err(e.context("the recording stopped"));
        }
        played
    }

    /// Record the decoded audio to `path` (WAV, or FLAC with a `.flac` name), ending a
    /// recording in progress first. The file is opened at once if audio is being
    /// decoded, else with the first audio. An error (the file cannot be created) ends
    /// the new recording at once and is returned.
    pub fn start_recording(&mut self, path: PathBuf) -> Result<()> {
        let ended = self.stop_recording();
        let mut r = Recorder::new(path);
        let opened = match self.format {
            Some(fmt) => r.open(fmt),
            None => Ok(()),
        };
        if let Err(e) = &opened {
            r.fail(e);
        }
        self.recorder = Some(r);
        ended.and(opened)
    }

    /// End the recording in progress, completing its file; its final state stays
    /// available ([`Self::recording`]).
    pub fn stop_recording(&mut self) -> Result<()> {
        match self.recorder.as_mut().filter(|r| r.active) {
            Some(r) => r.stop(),
            None => Ok(()),
        }
    }

    /// The recording in progress, or the last one (its final state).
    pub fn recording(&self) -> Option<RecordingStatus> {
        self.recorder.as_ref().map(Recorder::status)
    }

    /// Queue depth and drift correction for the status display.
    pub fn status(&self) -> Option<(Duration, f64)> {
        self.player.as_ref().map(|p| (p.buffered(), p.ppm()))
    }

    /// The smoothed spectrum of the audio pushed so far.
    pub fn spectrum(&self) -> AudioSpectrum {
        self.analyser.spectrum()
    }

    /// Finish: let queued audio play out and complete the recording.
    pub fn finish(&mut self) -> Result<()> {
        if let Some(p) = self.player.as_mut() {
            p.drain(Duration::from_secs(3));
        }
        self.stop_recording()
    }
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

    /// A recording: started while audio is decoded, the file opens at once; a change of
    /// format continues in a numbered file (skipping one that exists); stopped, the
    /// files are complete and the final state stays.
    #[test]
    fn recording_follows_the_format_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.wav");
        std::fs::write(dir.path().join("rec-2.wav"), b"someone else's").unwrap();
        let mut out = AudioOut::new(false, None, false, None).unwrap();
        assert!(out.recording().is_none());
        out.push(&tone(10, 0.5, 2400, 1), 24_000, 1).unwrap();
        out.start_recording(path.clone()).unwrap();
        let r = out.recording().unwrap();
        assert!(r.active && path.exists(), "opened at once: {r:?}");
        assert_eq!((r.files.len(), r.seconds, r.format), (1, 0.0, Some((24_000, 1))));
        out.push(&tone(10, 0.5, 24_000, 1), 24_000, 1).unwrap();
        out.push(&tone(10, 0.5, 4800, 2), 48_000, 2).unwrap();
        let r = out.recording().unwrap();
        let part = dir.path().join("rec-3.wav");
        assert_eq!(r.files, [path.clone(), part.clone()]);
        assert!((r.seconds - 1.1).abs() < 1e-9, "{}", r.seconds);
        assert_eq!(r.format, Some((48_000, 2)));
        out.stop_recording().unwrap();
        out.push(&tone(10, 0.5, 4800, 2), 48_000, 2).unwrap();
        let r = out.recording().unwrap();
        assert!(!r.active && r.error.is_none() && r.format.is_none(), "{r:?}");
        assert!((r.seconds - 1.1).abs() < 1e-9, "nothing after the stop: {}", r.seconds);
        assert_eq!(r.describe(), "1.1 s of audio in rec.wav, rec-3.wav");
        let frames = |p: &Path| decdrm_io::FileReader::open(p).unwrap().total_frames();
        assert_eq!(frames(&path), Some(24_000));
        assert_eq!(frames(&part), Some(4800));
        assert_eq!(std::fs::read(dir.path().join("rec-2.wav")).unwrap(), b"someone else's");
    }

    /// With the RF monitor on, a recording still holds the decoded audio, not the input.
    #[test]
    fn monitor_keeps_the_recording_on_the_decoded_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.wav");
        let mut out = AudioOut::new(false, None, false, Some(path.clone())).unwrap();
        out.set_monitor(true);
        assert!(out.monitoring());
        // 100 ms of I/Q input: played (with a sound card), not recorded.
        out.push_monitor(&tone(10, 0.5, 4800, 2), 2).unwrap();
        out.push(&tone(10, 0.5, 2400, 1), 24_000, 1).unwrap();
        out.set_monitor(false);
        out.push(&tone(10, 0.5, 2400, 1), 24_000, 1).unwrap();
        out.stop_recording().unwrap();
        let r = out.recording().unwrap();
        assert!((r.seconds - 0.2).abs() < 1e-9, "{r:?}");
        assert_eq!(decdrm_io::FileReader::open(&path).unwrap().total_frames(), Some(4800));
    }

    /// Started before any audio, the file opens with the first audio; a file that
    /// cannot be created ends the recording with the error.
    #[test]
    fn recording_waits_for_audio_and_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("later.flac");
        let mut out = AudioOut::new(false, None, false, Some(path.clone())).unwrap();
        let r = out.recording().unwrap();
        assert!(r.active && r.files.is_empty() && !path.exists());
        out.push(&tone(10, 0.5, 12_000, 2), 24_000, 2).unwrap();
        out.finish().unwrap();
        assert_eq!(decdrm_io::FileReader::open(&path).unwrap().total_frames(), Some(12_000));
        assert!(!out.recording().unwrap().active, "finish ends the recording");

        let bad = dir.path().join("no such folder").join("x.wav");
        let mut out = AudioOut::new(false, None, false, None).unwrap();
        out.push(&tone(10, 0.5, 100, 1), 24_000, 1).unwrap();
        assert!(out.start_recording(bad.clone()).is_err());
        let r = out.recording().unwrap();
        assert!(!r.active && r.error.as_deref().is_some_and(|e| e.contains("cannot record to")), "{r:?}");
        out.push(&tone(10, 0.5, 100, 1), 24_000, 1).unwrap();
        // The same error with the first audio when nothing was playing at the start.
        let mut out = AudioOut::new(false, None, false, Some(bad)).unwrap();
        let e = out.push(&tone(10, 0.5, 100, 1), 24_000, 1).unwrap_err();
        assert!(format!("{e:#}").starts_with("the recording stopped: cannot record to"), "{e:#}");
        assert!(out.push(&tone(10, 0.5, 100, 1), 24_000, 1).is_ok(), "reported once");
        assert!(!out.recording().unwrap().active);
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
