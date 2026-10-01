//! Signal sources: recordings, sound cards and KiwiSDRs, delivered as interleaved
//! `f32` frames at the 48 kHz working rate.

use anyhow::{Context, Result};
use decdrm_io::{FileReader, InputOptions, InputStream, To48k};
use decdrm_kiwi::{KiwiConfig, KiwiStatus, KiwiStream};
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
    /// A KiwiSDR on the internet tuned to a DRM frequency: I/Q (I left, Q right) at the
    /// Kiwi's rate, about 12 kHz.
    Kiwi(KiwiConfig),
}

impl InputSpec {
    /// A live input (sound card or KiwiSDR) rather than a recording.
    pub fn is_live(&self) -> bool {
        !matches!(self, InputSpec::File { .. })
    }

    /// The input is always I/Q, whatever the receiver's format setting says.
    pub fn is_iq(&self) -> bool {
        matches!(self, InputSpec::Kiwi(_))
    }
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
    /// Conversion to 48 kHz; for a KiwiSDR made once it has reported its rate.
    to48: Option<To48k>,
    info: SourceInfo,
    /// Frames delivered so far (at the source rate).
    pub frames_read: u64,
}

enum Kind {
    File(FileReader),
    Device(InputStream),
    Kiwi(KiwiStream),
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
            InputSpec::Kiwi(cfg) => {
                // Connects in the background: `read` waits for the samples, so a stop
                // request is never held up by a slow or unreachable KiwiSDR.
                let info = SourceInfo {
                    name: format!("KiwiSDR {} at {:.3} kHz", cfg.address, cfg.freq_khz),
                    // Nominal until the Kiwi reports its rate.
                    sample_rate: 12_000,
                    channels: 2,
                    duration_s: None,
                    is_file: false,
                };
                return Ok(Self { kind: Kind::Kiwi(KiwiStream::start(cfg.clone())), to48: None, info, frames_read: 0 });
            }
        };
        let to48 = To48k::new(info.sample_rate, info.channels).context("creating resampler")?;
        Ok(Self { kind, to48: Some(to48), info, frames_read: 0 })
    }

    pub fn info(&self) -> &SourceInfo {
        &self.info
    }

    /// Seconds of input consumed so far.
    pub fn position_s(&self) -> f64 {
        self.frames_read as f64 / f64::from(self.info.sample_rate.max(1))
    }

    /// Connection events of a KiwiSDR input since the last call (for the log).
    pub fn take_log(&self) -> Vec<String> {
        match &self.kind {
            Kind::Kiwi(s) => s.take_log(),
            _ => Vec::new(),
        }
    }

    /// The state of a KiwiSDR input.
    pub fn kiwi_status(&self) -> Option<KiwiStatus> {
        match &self.kind {
            Kind::Kiwi(s) => Some(s.status()),
            _ => None,
        }
    }

    /// Read up to `max_frames` source frames and return them converted to 48 kHz.
    /// `Ok(None)` means end of file. For live inputs this waits briefly for data; a
    /// KiwiSDR connection that has ended for good is an error (its reason).
    pub fn read(&mut self, max_frames: usize) -> Result<Option<Vec<f32>>> {
        let raw = match &mut self.kind {
            Kind::File(r) => match r.read(max_frames)? {
                Some(v) => v,
                None => {
                    let tail = self.to48.as_mut().map(To48k::flush).unwrap_or_default();
                    return Ok(if tail.is_empty() { None } else { Some(tail) });
                }
            },
            Kind::Device(s) => {
                if s.is_dead() {
                    anyhow::bail!("sound card input stopped");
                }
                s.read_blocking(max_frames, Duration::from_millis(500)).unwrap_or_default()
            }
            Kind::Kiwi(s) => {
                let raw = s.read_blocking(max_frames, Duration::from_millis(500)).map_err(|e| anyhow::anyhow!("{e}"))?;
                // The resampler follows the Kiwi's reported rate (rounded to 1 Hz; the
                // receiver tracks the rest), also after a redirection to another Kiwi.
                if let Some(rate) = s.sample_rate() {
                    let rate = rate.round() as u32;
                    if self.to48.is_none() || rate != self.info.sample_rate {
                        self.to48 = Some(To48k::new(rate, 2).context("creating resampler")?);
                        self.info.sample_rate = rate;
                    }
                }
                raw
            }
        };
        self.frames_read += (raw.len() / self.info.channels.max(1)) as u64;
        Ok(Some(match &mut self.to48 {
            Some(c) => c.process(&raw),
            None => Vec::new(),
        }))
    }
}
