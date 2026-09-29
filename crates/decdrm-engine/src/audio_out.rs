//! Audio outputs of the engine: live playback (drift-compensated) and recording.

use anyhow::{Context, Result};
use decdrm_io::{AudioFormat, AudioPlayer, Container, Encoding, FileWriter, PlayerOptions};
use std::path::PathBuf;
use std::time::Duration;

/// Where decoded audio goes.
pub struct AudioOut {
    player: Option<AudioPlayer>,
    /// Pace pushes to the sound card (file playback) instead of dropping excess
    /// audio (live reception, where the drift loop keeps the queue on target).
    blocking: bool,
    recorder: Option<Recorder>,
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
        Ok(Self { player, blocking, recorder: record.map(|path| Recorder { path, writer: None, format: None }) })
    }

    pub fn is_playing(&self) -> bool {
        self.player.is_some()
    }

    /// Queue decoded audio (interleaved, `channels` = 1 or 2).
    pub fn push(&mut self, samples: &[f32], sample_rate: u32, channels: usize) -> Result<()> {
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
