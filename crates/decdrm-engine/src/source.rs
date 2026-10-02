//! Signal sources: recordings, sound cards and KiwiSDRs, delivered as interleaved
//! `f32` frames at the 48 kHz working rate; for diversity reception two of them side
//! by side. MDI/RSCI input delivers multiplex frames instead ([`Input::Mdi`]).

use crate::snapshot::MdiStatus;
use anyhow::{Context, Result};
use decdrm_io::{FileReader, InputOptions, InputStream, To48k};
use decdrm_kiwi::{KiwiConfig, KiwiStatus, KiwiStream};
use decdrm_mdi::net::UdpDestination;
use decdrm_mdi::rci::{RciCommand, RciSender};
use decdrm_mdi::source::{MdiInput, MdiOrigin, MdiRead};
use decdrm_mdi::{MdiFrame, RsciStatus};
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
    /// MDI or RSCI (`decdrm_mdi`) from UDP or a recording: the multiplex as decoded
    /// elsewhere — by a content server, or an RSCI receiver with its status — without
    /// the radio part.
    Mdi(MdiSpec),
}

/// An MDI/RSCI input.
#[derive(Debug, Clone)]
pub struct MdiSpec {
    pub origin: MdiOrigin,
    /// A recording: read at 400 ms per frame, as a live source sends, rather than as
    /// fast as possible.
    pub realtime: bool,
    /// Send RCI commands (tune, select a service) to the RSCI receiver here.
    pub rci: Option<UdpDestination>,
}

impl InputSpec {
    /// A live input (sound card, KiwiSDR, MDI over UDP) rather than a recording.
    pub fn is_live(&self) -> bool {
        match self {
            InputSpec::File { .. } => false,
            InputSpec::Diversity(b) => b.iter().any(InputSpec::is_live),
            InputSpec::Mdi(m) => matches!(m.origin, MdiOrigin::Udp(_)),
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
            InputSpec::Mdi(m) => m.realtime && matches!(m.origin, MdiOrigin::File { .. }),
            _ => false,
        }
    }

    /// MDI/RSCI input (no samples: multiplex frames).
    pub fn is_mdi(&self) -> bool {
        matches!(self, InputSpec::Mdi(_))
    }
}

/// What one read of a [`Source`] gave.
pub enum Input {
    /// 48 kHz frames per branch (one, or two for diversity reception); empty when
    /// nothing came.
    Samples(Vec<Vec<f32>>),
    /// MDI/RSCI frames (none when nothing came).
    Mdi(Vec<MdiFrame>),
}

/// An MDI/RSCI input with what the status shows of it.
struct MdiSource {
    input: MdiInput,
    /// Sends RCI to the RSCI receiver.
    rci: Option<RciSender>,
    frames: u64,
    /// Protocol of the first frame ("MDI" / "RSCI" and revision).
    protocol: Option<String>,
    /// The latest RSCI receiver status.
    rsci: RsciStatus,
    log: Vec<String>,
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
    Mdi(Box<MdiSource>),
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
            InputSpec::Mdi(spec) => {
                let input = MdiInput::open(spec.origin.clone(), spec.realtime)
                    .with_context(|| format!("opening MDI/RSCI input {}", spec.origin.describe()))?;
                let rci = match &spec.rci {
                    Some(d) => Some(RciSender::new(d).with_context(|| format!("RCI destination {}", d.addr))?),
                    None => None,
                };
                let info = SourceInfo {
                    name: format!("MDI/RSCI {}", spec.origin.describe()),
                    sample_rate: 0,
                    channels: 0,
                    duration_s: None,
                    is_file: input.is_file(),
                };
                let mut log = vec![format!("MDI/RSCI: listening on {}", spec.origin.describe())];
                if input.is_file() {
                    log[0] = format!("MDI/RSCI: reading {}", spec.origin.describe());
                }
                if let Some(r) = &rci {
                    log.push(format!("MDI/RSCI: RCI commands go to {}", r.destination()));
                }
                let kind = Kind::Mdi(Box::new(MdiSource { input, rci, frames: 0, protocol: None, rsci: RsciStatus::default(), log }));
                return Ok(Self { kind, to48: None, info, frames_read: 0 });
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

    /// Seconds of input consumed so far (diversity reception: of the first branch;
    /// MDI: 400 ms per frame).
    pub fn position_s(&self) -> f64 {
        match &self.kind {
            Kind::Pair(s, _) => s[0].position_s(),
            Kind::Mdi(m) => m.frames as f64 * 0.4,
            _ => self.frames_read as f64 / f64::from(self.info.sample_rate.max(1)),
        }
    }

    /// The state of an MDI/RSCI input.
    pub fn mdi_status(&self) -> Option<MdiStatus> {
        let Kind::Mdi(m) = &self.kind else { return None };
        Some(MdiStatus {
            origin: m.input.origin().describe(),
            protocol: m.protocol.clone(),
            local: m.input.local_addr().map(|a| a.to_string()),
            sender: m.input.last_sender.map(|a| a.to_string()),
            stats: m.input.receiver.stats,
            rci: m.rci.as_ref().map(|r| r.destination().to_string()),
            rsci: m.rsci.clone(),
            progress: m.input.progress().filter(|&(_, size)| size > 0).map(|(read, size)| read as f64 / size as f64),
        })
    }

    /// Send RCI commands to the RSCI receiver of an MDI/RSCI input (if one is set up);
    /// `false` if they could not go anywhere.
    pub fn send_rci(&mut self, commands: &[RciCommand]) -> bool {
        let Kind::Mdi(m) = &mut self.kind else { return false };
        let Some(rci) = m.rci.as_mut() else { return false };
        let what: Vec<String> = commands.iter().map(RciCommand::describe).collect();
        match rci.send(commands) {
            Ok(()) => m.log.push(format!("RCI to {}: {}", rci.destination(), what.join(", "))),
            Err(e) => m.log.push(format!("RCI to {}: {e}", rci.destination())),
        }
        true
    }

    /// Connection events of a KiwiSDR input since the last call (for the log); a
    /// diversity input's are numbered ("KiwiSDR 2: …").
    pub fn take_log(&mut self) -> Vec<String> {
        match &mut self.kind {
            Kind::Mdi(m) => std::mem::take(&mut m.log),
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
    /// diversity input, or an RSCI receiver by RCI; `false` for an input that cannot
    /// be tuned.
    pub fn tune(&mut self, freq_khz: f64) -> bool {
        if matches!(self.kind, Kind::Mdi(_)) {
            return self.send_rci(&[RciCommand::Frequency((freq_khz * 1000.0).round() as u32)]);
        }
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
    /// Read what comes next: up to `max_frames` source frames of every branch
    /// ([`Self::read_branches`]), or the MDI/RSCI frames that arrived within about as
    /// long. `Ok(None)` means the end of the input.
    pub fn read_input(&mut self, max_frames: usize) -> Result<Option<Input>> {
        let Kind::Mdi(m) = &mut self.kind else {
            return Ok(self.read_branches(max_frames)?.map(Input::Samples));
        };
        let wait = Duration::from_secs_f64((max_frames as f64 / 48_000.0).clamp(0.005, 0.1));
        Ok(match m.input.read(wait)? {
            MdiRead::Frame(f) => {
                m.frames += 1;
                if m.protocol.is_none() {
                    let p = f.protocol.map_or_else(
                        || "MDI (no *ptr)".to_string(),
                        |p| format!("{} {}.{}", String::from_utf8_lossy(&p.name), p.major, p.minor),
                    );
                    let from = m.input.last_sender.map(|a| format!(" from {a}")).unwrap_or_default();
                    m.log.push(format!("MDI/RSCI: first frame{from}: {p}{}", f.rsci.profile.map(|c| format!(", profile {c}")).unwrap_or_default()));
                    m.protocol = Some(p);
                }
                if f.is_rsci() {
                    m.rsci = f.rsci.clone();
                }
                Some(Input::Mdi(vec![*f]))
            }
            MdiRead::Idle => Some(Input::Mdi(Vec::new())),
            MdiRead::End => None,
        })
    }

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
            Kind::Mdi(_) => anyhow::bail!("an MDI/RSCI input has no samples"),
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
