//! Signal sources: recordings and sound cards, delivered as interleaved `f32`
//! frames at the 48 kHz working rate.

use anyhow::{Context, Result};
use decdrm_io::{FileReader, InputOptions, InputStream, To48k};
use std::path::PathBuf;
use std::time::Duration;

/// Where the signal comes from.
#[derive(Debug, Clone)]
pub enum InputSpec {
    /// A WAV/FLAC recording. `realtime` paces reading to wall-clock time (for
    /// listening); otherwise the file is decoded as fast as possible.
    File { path: PathBuf, realtime: bool },
    /// A sound card input (e.g. a virtual audio cable). `channels` = 1 for a real IF,
    /// 2 for I/Q; `None` uses the device default.
    Device { name: Option<String>, channels: Option<usize> },
}

/// Static information about an opened source.
#[derive(Debug, Clone, Default)]
pub struct SourceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub duration_s: Option<f64>,
    pub is_file: bool,
}

/// An opened source producing 48 kHz frames.
pub struct Source {
    kind: Kind,
    to48: To48k,
    info: SourceInfo,
    /// Frames delivered so far (at the source rate).
    pub frames_read: u64,
}

enum Kind {
    File(FileReader),
    Device(InputStream),
}

impl Source {
    pub fn open(spec: &InputSpec) -> Result<Self> {
        let (kind, info) = match spec {
            InputSpec::File { path, .. } => {
                let r = FileReader::open(path).with_context(|| format!("opening {}", path.display()))?;
                let f = r.format();
                let info = SourceInfo {
                    name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    sample_rate: f.sample_rate,
                    channels: f.channels,
                    duration_s: r.duration().map(|d| d.as_secs_f64()),
                    is_file: true,
                };
                (Kind::File(r), info)
            }
            InputSpec::Device { name, channels } => {
                let opts = InputOptions {
                    device: name.clone(),
                    sample_rate: Some(48_000),
                    channels: *channels,
                    buffer: Duration::from_secs(3),
                };
                let s = InputStream::open(&opts).context("opening sound card input")?;
                let f = s.format();
                let info = SourceInfo {
                    name: s.device_name().to_string(),
                    sample_rate: f.sample_rate,
                    channels: f.channels,
                    duration_s: None,
                    is_file: false,
                };
                (Kind::Device(s), info)
            }
        };
        let to48 = To48k::new(info.sample_rate, info.channels).context("creating resampler")?;
        Ok(Self { kind, to48, info, frames_read: 0 })
    }

    pub fn info(&self) -> &SourceInfo {
        &self.info
    }

    /// Seconds of input consumed so far.
    pub fn position_s(&self) -> f64 {
        self.frames_read as f64 / f64::from(self.info.sample_rate.max(1))
    }

    /// Read up to `max_frames` source frames and return them converted to 48 kHz.
    /// `Ok(None)` means end of file. For devices this waits briefly for data.
    pub fn read(&mut self, max_frames: usize) -> Result<Option<Vec<f32>>> {
        let raw = match &mut self.kind {
            Kind::File(r) => match r.read(max_frames)? {
                Some(v) => v,
                None => {
                    let tail = self.to48.flush();
                    return Ok(if tail.is_empty() { None } else { Some(tail) });
                }
            },
            Kind::Device(s) => {
                if s.is_dead() {
                    anyhow::bail!("sound card input stopped");
                }
                s.read_blocking(max_frames, Duration::from_millis(500)).unwrap_or_default()
            }
        };
        self.frames_read += (raw.len() / self.info.channels.max(1)) as u64;
        Ok(Some(self.to48.process(&raw)))
    }
}
