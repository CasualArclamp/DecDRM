//! Signal sources: recordings, sound cards and KiwiSDRs, delivered as interleaved
//! `f32` frames at the 48 kHz working rate; for diversity reception two of them side
//! by side.

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
    /// Diversity reception: the same station from two inputs (two KiwiSDRs far apart,
    /// say), combined before decoding (see `decdrm_core::rx::diversity`).
    Diversity(Box<[InputSpec; 2]>),
}

impl InputSpec {
    /// A live input (sound card or KiwiSDR) rather than a recording.
    pub fn is_live(&self) -> bool {
        match self {
            InputSpec::File { .. } => false,
            InputSpec::Diversity(b) => b.iter().any(InputSpec::is_live),
            _ => true,
        }
    }

    /// The input is always I/Q, whatever the receiver's format setting says (for
    /// diversity reception: both branches).
    pub fn is_iq(&self) -> bool {
        match self {
            InputSpec::Kiwi(_) => true,
            InputSpec::Diversity(b) => b.iter().all(InputSpec::is_iq),
            _ => false,
        }
    }

    /// A recording read at real-time pace (for diversity reception: either branch).
    pub fn is_realtime(&self) -> bool {
        match self {
            InputSpec::File { realtime, .. } => *realtime,
            InputSpec::Diversity(b) => b.iter().any(InputSpec::is_realtime),
            _ => false,
        }
    }
}

/// How long a branch of a diversity input is waited for per read: short, so a slow
/// or dead branch does not hold up the other.
const BRANCH_WAIT: Duration = Duration::from_millis(100);

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
    /// The stream and the address it was opened with (for the name).
    Kiwi(KiwiStream, String),
    /// Diversity reception: two sources, and which of them has ended (and why).
    Pair(Box<[Source; 2]>, [Option<String>; 2]),
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
            InputSpec::Diversity(specs) => {
                let a = Source::open(&specs[0]).context("opening the first input")?;
                let b = Source::open(&specs[1]).context("opening the second input")?;
                let info = SourceInfo { name: format!("{} + {}", a.info.name, b.info.name), ..a.info.clone() };
                return Ok(Self { kind: Kind::Pair(Box::new([a, b]), [None, None]), to48: None, info, frames_read: 0 });
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
                let kind = Kind::Kiwi(KiwiStream::start(cfg.clone()), cfg.address.to_string());
                return Ok(Self { kind, to48: None, info, frames_read: 0 });
            }
        };
        let to48 = To48k::new(info.sample_rate, info.channels).context("creating resampler")?;
        Ok(Self { kind, to48: Some(to48), info, frames_read: 0 })
    }

    pub fn info(&self) -> &SourceInfo {
        &self.info
    }

    /// Information about input `branch` of a diversity input (otherwise the input's).
    pub fn branch_info(&self, branch: usize) -> &SourceInfo {
        match &self.kind {
            Kind::Pair(s, _) => s[branch.min(1)].info(),
            _ => &self.info,
        }
    }

    /// Seconds of input consumed so far (diversity reception: of the first branch).
    pub fn position_s(&self) -> f64 {
        match &self.kind {
            Kind::Pair(s, _) => s[0].position_s(),
            _ => self.frames_read as f64 / f64::from(self.info.sample_rate.max(1)),
        }
    }

    /// Connection events of a KiwiSDR input since the last call (for the log); a
    /// diversity input's are numbered ("KiwiSDR 2: …").
    pub fn take_log(&self) -> Vec<String> {
        match &self.kind {
            Kind::Kiwi(s, _) => s.take_log(),
            Kind::Pair(s, _) => (0..2)
                .flat_map(|b| {
                    s[b].take_log().into_iter().map(move |l| match l.strip_prefix("KiwiSDR:") {
                        Some(rest) => format!("KiwiSDR {}:{rest}", b + 1),
                        None => format!("input {}: {l}", b + 1),
                    })
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The state of a KiwiSDR input (diversity reception: of the first branch).
    pub fn kiwi_status(&self) -> Option<KiwiStatus> {
        self.kiwi_status_of(0)
    }

    /// The state of KiwiSDR `branch` of a diversity input (0: also a plain KiwiSDR
    /// input).
    pub fn kiwi_status_of(&self, branch: usize) -> Option<KiwiStatus> {
        match &self.kind {
            Kind::Kiwi(s, _) if branch == 0 => Some(s.status()),
            Kind::Pair(s, _) => s[branch.min(1)].kiwi_status_of(0),
            _ => None,
        }
    }

    /// Retune a KiwiSDR input to `freq_khz` (see [`KiwiStream::tune`]), both of a
    /// diversity input; `false` for an input that cannot be tuned.
    pub fn tune(&mut self, freq_khz: f64) -> bool {
        match &mut self.kind {
            Kind::Pair(s, _) => {
                let tuned = [s[0].tune(freq_khz), s[1].tune(freq_khz)];
                self.info.name = format!("{} + {}", s[0].info.name, s[1].info.name);
                tuned[0] || tuned[1]
            }
            Kind::Kiwi(s, address) => {
                s.tune(freq_khz);
                self.info.name = format!("KiwiSDR {address} at {freq_khz:.3} kHz");
                // A fresh resampler: its history belongs to the old frequency.
                self.to48 = None;
                true
            }
            _ => false,
        }
    }

    /// Read up to `max_frames` source frames of every branch (one, or two for a
    /// diversity input), converted to 48 kHz. `Ok(None)` means the end of the input
    /// (of both branches); a branch that ended or had nothing gives an empty vector. A
    /// diversity input carries on with one branch when the other ends; its error is
    /// returned once both have.
    pub fn read_branches(&mut self, max_frames: usize) -> Result<Option<Vec<Vec<f32>>>> {
        let Kind::Pair(s, ended) = &mut self.kind else {
            return Ok(self.read(max_frames)?.map(|v| vec![v]));
        };
        let mut out = vec![Vec::new(), Vec::new()];
        for b in 0..2 {
            if ended[b].is_some() {
                continue;
            }
            match s[b].read_waiting(max_frames, BRANCH_WAIT) {
                Ok(Some(v)) => out[b] = v,
                Ok(None) => ended[b] = Some(String::new()),
                Err(e) => ended[b] = Some(format!("{e:#}")),
            }
        }
        match (&ended[0], &ended[1]) {
            (Some(a), Some(b)) if a.is_empty() && b.is_empty() => Ok(None),
            (Some(a), Some(b)) => anyhow::bail!("{}", [a.as_str(), b.as_str()].iter().filter(|e| !e.is_empty()).copied().collect::<Vec<_>>().join("; ")),
            _ => Ok(Some(out)),
        }
    }

    /// Read up to `max_frames` source frames and return them converted to 48 kHz.
    /// `Ok(None)` means end of file. For live inputs this waits briefly for data; a
    /// KiwiSDR connection that has ended for good is an error (its reason). A diversity
    /// input gives its first branch here; see [`Self::read_branches`].
    pub fn read(&mut self, max_frames: usize) -> Result<Option<Vec<f32>>> {
        self.read_waiting(max_frames, Duration::from_millis(500))
    }

    /// [`Self::read`], waiting up to `wait` for live input.
    fn read_waiting(&mut self, max_frames: usize, wait: Duration) -> Result<Option<Vec<f32>>> {
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
                s.read_blocking(max_frames, wait).unwrap_or_default()
            }
            Kind::Pair(..) => {
                return Ok(self.read_branches(max_frames)?.map(|mut v| v.swap_remove(0)));
            }
            Kind::Kiwi(s, _) => {
                let raw = s.read_blocking(max_frames, wait).map_err(|e| anyhow::anyhow!("{e}"))?;
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
