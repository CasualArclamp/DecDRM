//! The modulator: a station that transmits MDI (TS 102 820) from a content server — a
//! DRM multiplex made elsewhere — instead of its own services (`[mdi]` in the station
//! file).
//!
//! A thread reads the MDI (UDP or a recording, see `decdrm_mdi`) into a queue. For
//! each transmission frame the modulator takes the frame for the transmitter's
//! position in the super frame, which the FAC's frame identity names: its FAC goes out
//! as sent, its SDC block in the first frame of a super frame, and its streams are
//! multiplexed by the stream lengths of its `sdci` item. `robm`, the FAC and `sdci` set
//! the channel; a change rebuilds the transmitter at the start of a super frame.
//!
//! Gaps: a frame that is lost, late or damaged is replaced by a filler — the previous
//! FAC, an empty MSC (receivers conceal the audio), the previous SDC — so the signal
//! never stops; the logical frame count (`dlfc`) recognises late frames. With a sound
//! card the queue first holds `buffer_frames` frames, a reserve against network
//! jitter, and is trimmed by whole super frames when the content server's clock runs
//! ahead of the sound card's (an empty queue gives fillers when it runs behind).

use crate::config::MdiSettings;
use crate::error::StationError;
use decdrm_core::fac::Fac;
use decdrm_core::fec::mlc::MscProtection;
use decdrm_core::mux::msc::{MscGeometry, multiplex};
use decdrm_core::mux::sdc::{MultiplexDescription, StreamDescription};
use decdrm_core::mux::service::{AudioCodec, AudioMode, Ensemble};
use decdrm_core::params::RobustnessMode;
use decdrm_core::tx::{Transmitter, TxConfig};
use decdrm_mdi::source::{MdiInput, MdiOrigin, MdiRead};
use decdrm_mdi::{DcpStats, MdiFrame};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long one call waits for MDI before giving up for now.
const WAIT: Duration = Duration::from_millis(100);
/// Frames the feed thread may read ahead of the transmission.
const QUEUE: usize = 64;

/// The state of a modulator, for user interfaces.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModulatorStatus {
    /// Where the MDI comes from ("UDP port 8000", "studio.pcap").
    pub input: String,
    /// UDP: the content server (sender of the last packet).
    pub sender: Option<String>,
    /// Protocol and revision of the first frame ("DMDI 0.0").
    pub protocol: Option<String>,
    /// Waiting for the first frame that starts a super frame.
    pub waiting: bool,
    /// MDI frames transmitted.
    pub frames: u64,
    /// Frames transmitted without MDI (lost, late or damaged frames).
    pub fillers: u64,
    /// Frames thrown away: late, damaged, or trimmed when the queue grew too long.
    pub dropped: u64,
    /// Frames waiting in the queue.
    pub queued: usize,
    /// The link: packets, frames, losses, PFT recoveries.
    pub link: DcpStats,
    /// The channel being transmitted, e.g. "mode B, 10 kHz, 64-QAM, short interleaving".
    pub channel: Option<String>,
    /// The recording has been read to the end.
    pub ended: bool,
}

/// What the station transmits next.
pub(crate) enum Job {
    /// A frame: with this channel configuration, FAC, multiplex frame (bits) and SDC
    /// data field.
    Frame { config: TxConfig, fac: Fac, msc: Vec<u8>, sdc: Option<Vec<u8>> },
    /// Not transmitting yet, but a sound card wants samples: 400 ms of silence.
    Silence,
    /// Nothing yet (no sound card to feed): try again.
    Nothing,
}

enum Feed {
    Frame(Box<MdiFrame>),
    End,
    Error(String),
}

/// What the feed thread shares besides the frames.
#[derive(Default)]
struct Link {
    stats: DcpStats,
    sender: Option<String>,
}

/// See the module docs.
pub(crate) struct Modulator {
    queue: Receiver<Feed>,
    feed_stop: Arc<AtomicBool>,
    feed: Option<JoinHandle<()>>,
    link: Arc<Mutex<Link>>,
    buffer: VecDeque<MdiFrame>,
    /// A sound card paces the transmission (else: as fast as the MDI comes).
    realtime: bool,
    buffer_frames: usize,
    is_file: bool,
    started: bool,
    /// The configuration being transmitted, and its MSC geometry.
    current: Option<(TxConfig, MscGeometry)>,
    last_fac: Option<Fac>,
    next_dlfc: Option<u32>,
    /// The services, from the FAC and SDC (for the status).
    ensemble: Ensemble,
    pub status: ModulatorStatus,
    log: Vec<String>,
}

impl Modulator {
    /// Start reading the MDI of `settings` (relative recording paths against
    /// `base_dir`). `realtime`: a sound card paces the transmission.
    pub fn new(settings: &MdiSettings, base_dir: Option<&Path>, realtime: bool) -> Result<Self, StationError> {
        let origin = origin(settings, base_dir).map_err(StationError::Mdi)?;
        let input = MdiInput::open(origin.clone(), false).map_err(|e| StationError::Mdi(format!("{}: {e}", origin.describe())))?;
        let is_file = input.is_file();
        let (tx, rx) = sync_channel(QUEUE);
        let feed_stop = Arc::new(AtomicBool::new(false));
        let link = Arc::new(Mutex::new(Link::default()));
        let feed = {
            let (stop, link) = (Arc::clone(&feed_stop), Arc::clone(&link));
            std::thread::Builder::new()
                .name("decdrm-mdi-in".into())
                .spawn(move || feed(input, tx, stop, link))
                .map_err(|e| StationError::Mdi(format!("cannot start the MDI input thread: {e}")))?
        };
        let what = origin.describe();
        Ok(Self {
            queue: rx,
            feed_stop,
            feed: Some(feed),
            link,
            buffer: VecDeque::new(),
            realtime,
            buffer_frames: settings.buffer_frames.max(1),
            is_file,
            started: false,
            current: None,
            last_fac: None,
            next_dlfc: None,
            ensemble: Ensemble::new(),
            status: ModulatorStatus { input: what.clone(), waiting: true, ..ModulatorStatus::default() },
            log: vec![format!("modulator: MDI from {what}")],
        })
    }

    /// The input is a recording (it ends by itself).
    pub fn is_finite(&self) -> bool {
        self.is_file
    }

    /// The recording has been read and transmitted to the end.
    pub fn finished(&self) -> bool {
        self.status.ended && self.buffer.is_empty()
    }

    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    /// The services of the multiplex being transmitted, from its FAC and SDC.
    pub fn ensemble(&self) -> &Ensemble {
        &self.ensemble
    }

    /// Move what the feed delivered into the buffer; `wait`: block up to that long for
    /// the first item.
    fn drain(&mut self, wait: Option<Duration>) -> Result<(), StationError> {
        let mut first = wait;
        loop {
            let item = match first.take() {
                Some(w) => match self.queue.recv_timeout(w) {
                    Ok(i) => i,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => Feed::End,
                },
                None => match self.queue.try_recv() {
                    Ok(i) => i,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => Feed::End,
                },
            };
            match item {
                Feed::Frame(f) => {
                    if self.status.protocol.is_none() {
                        let p = f.protocol.map_or_else(
                            || "MDI (no *ptr)".to_string(),
                            |p| format!("{} {}.{}", String::from_utf8_lossy(&p.name), p.major, p.minor),
                        );
                        self.log.push(format!("modulator: first MDI frame ({p})"));
                        self.status.protocol = Some(p);
                    }
                    self.buffer.push_back(*f);
                }
                Feed::End => {
                    if !self.status.ended {
                        self.status.ended = true;
                        if self.is_file {
                            self.log.push("modulator: end of the MDI recording".into());
                        }
                    }
                    break;
                }
                Feed::Error(e) => return Err(StationError::Mdi(e)),
            }
        }
        if let Ok(l) = self.link.lock() {
            self.status.link = l.stats;
            self.status.sender.clone_from(&l.sender);
        }
        self.status.queued = self.buffer.len();
        Ok(())
    }

    /// Whether `f` was due before the frame being transmitted now (by its frame count).
    fn late(&self, f: &MdiFrame) -> bool {
        match (self.next_dlfc, f.dlfc) {
            (Some(next), Some(n)) => {
                let behind = next.wrapping_sub(n);
                // A count far away is a restart of the content server, not a late frame.
                behind != 0 && behind < 1000
            }
            _ => false,
        }
    }

    /// The content of the next transmission frame, at position `frame_index` of the
    /// super frame (the transmitter's).
    pub fn next(&mut self, frame_index: usize) -> Result<Job, StationError> {
        self.drain(None)?;
        if !self.started {
            // A frame starting a super frame, with the items a modulator needs.
            while let Some(f) = self.buffer.front() {
                if starts_super_frame(f) {
                    break;
                }
                self.buffer.pop_front();
            }
            let enough = self.buffer.len() >= if self.realtime { self.buffer_frames } else { 1 };
            if !(self.buffer.front().is_some() && (enough || self.status.ended)) {
                if self.status.ended && self.buffer.is_empty() {
                    return Err(StationError::Mdi(format!("{}: no frame that starts a super frame", self.status.input)));
                }
                if self.realtime {
                    return Ok(Job::Silence);
                }
                self.drain(Some(WAIT))?;
                return Ok(Job::Nothing);
            }
            if frame_index != 0 {
                // The transmitter is mid-super frame (a restart): fill up to its end.
                return Ok(self.filler());
            }
            self.started = true;
            self.status.waiting = false;
            self.log.push(format!("modulator: transmitting ({} frames queued)", self.buffer.len()));
        }
        loop {
            // Late and damaged frames go.
            while let Some(f) = self.buffer.front() {
                let fac_ok = f.fac_bits().and_then(|b| Fac::parse(&b)).is_some();
                if !fac_ok || self.late(f) {
                    self.buffer.pop_front();
                    self.status.dropped += 1;
                } else {
                    break;
                }
            }
            // The content server's clock runs ahead of the sound card's: drop whole
            // super frames (the frame identities stay in step).
            if self.realtime && self.buffer.len() >= 2 * self.buffer_frames + 3 {
                let n = (self.buffer.len() - self.buffer_frames) / 3 * 3;
                self.buffer.drain(..n);
                self.status.dropped += n as u64;
                if let Some(d) = self.next_dlfc.as_mut() {
                    *d = d.wrapping_add(n as u32);
                }
                self.log.push(format!("modulator: {n} frames dropped (the MDI comes faster than the sound card plays)"));
            }
            match self.buffer.front() {
                Some(f) => {
                    let fac = Fac::parse(&f.fac_bits().expect("checked above")).expect("checked above");
                    if usize::from(fac.channel.frame_index) != frame_index {
                        // A frame for a later position: frames were lost.
                        return Ok(self.filler());
                    }
                    let f = self.buffer.pop_front().expect("front exists");
                    match self.job(&f, fac, frame_index) {
                        Ok(job) => {
                            self.next_dlfc = f.dlfc.map(|n| n.wrapping_add(1));
                            self.status.frames += 1;
                            self.status.queued = self.buffer.len();
                            return Ok(job);
                        }
                        Err(e) => {
                            self.log.push(format!("modulator: frame {}: {e}", f.dlfc.map_or_else(|| "?".into(), |n| n.to_string())));
                            self.status.dropped += 1;
                            return Ok(self.filler());
                        }
                    }
                }
                None if self.status.ended || self.realtime => return Ok(self.filler()),
                None => {
                    self.drain(Some(WAIT))?;
                    if self.buffer.is_empty() && !self.status.ended {
                        return Ok(Job::Nothing);
                    }
                }
            }
        }
    }

    /// A frame without MDI: the previous FAC, an empty multiplex frame, the previous
    /// SDC (the transmitter repeats it).
    fn filler(&mut self) -> Job {
        let (Some((config, geometry)), Some(fac)) = (self.current, self.last_fac) else {
            return if self.realtime { Job::Silence } else { Job::Nothing };
        };
        self.status.fillers += 1;
        if let Some(d) = self.next_dlfc.as_mut() {
            *d = d.wrapping_add(1);
        }
        Job::Frame { config, fac, msc: vec![0; geometry.vspp_bits + geometry.hpp_bits + geometry.lpp_bits], sdc: None }
    }

    /// What to transmit for MDI frame `f` (FAC `fac`) at position `frame_index`.
    fn job(&mut self, f: &MdiFrame, fac: Fac, frame_index: usize) -> Result<Job, String> {
        let sdci = f.sdci.as_ref().ok_or("no sdci item")?;
        let ch = fac.channel;
        let mode = match f.robustness {
            Some(m) => RobustnessMode::ALL[usize::from(m.min(3))],
            None => self.current.map(|(c, _)| c.mode).ok_or("no robm item")?,
        };
        let hierarchical = ch.msc_mode.is_hierarchical();
        let mux = MultiplexDescription {
            protection_a: sdci.protection_a,
            protection_b: sdci.protection_b,
            streams: sdci.streams.iter().map(|&(a, b)| StreamDescription { len_a: a, len_b: b }).collect(),
        };
        let lengths = mux.streams(hierarchical);
        let part_a_bytes: usize =
            lengths.iter().enumerate().filter(|&(i, _)| !(hierarchical && i == 0)).map(|(_, l)| l.part_a).sum();
        let afs_index = f.sdc.as_ref().map(|s| s.afs_index).or(self.current.map(|(c, _)| c.afs_index)).unwrap_or(0);
        let config = TxConfig {
            mode,
            occupancy: ch.occupancy,
            msc_mode: ch.msc_mode,
            sdc_mode: ch.sdc_mode,
            interleaving: ch.interleaving,
            protection: MscProtection {
                part_a: usize::from(sdci.protection_a),
                part_b: usize::from(sdci.protection_b),
                hierarchical: if hierarchical { usize::from(sdci.hierarchical().map_or(0, |(p, _)| p)) } else { 0 },
            },
            part_a_bytes,
            afs_index,
        };
        let geometry = match self.current {
            Some((c, g)) if c == config => g,
            _ => {
                if frame_index != 0 {
                    return Err("the channel changes in the middle of a super frame".into());
                }
                let tx = Transmitter::new(config).map_err(|e| format!("cannot transmit this channel: {e}"))?;
                let cap = tx.msc_capacity();
                let g = MscGeometry { vspp_bits: cap.vspp_bits, hpp_bits: cap.hpp_bits, lpp_bits: cap.lpp_bits };
                let channel = format!(
                    "mode {}, {} kHz, {}, {} interleaving",
                    ["A", "B", "C", "D"][mode.index()],
                    ch.occupancy.bandwidth_khz(),
                    crate::config::MscModeSetting(ch.msc_mode),
                    crate::config::InterleavingSetting(ch.interleaving)
                );
                self.log.push(format!("modulator: channel {channel}"));
                self.status.channel = Some(channel);
                self.current = Some((config, g));
                g
            }
        };
        let streams: Vec<Vec<u8>> = lengths
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let mut d = f.streams.get(i).cloned().flatten().unwrap_or_default();
                d.resize(l.total(), 0);
                d
            })
            .collect();
        let refs: Vec<&[u8]> = streams.iter().map(Vec::as_slice).collect();
        let msc = multiplex(&refs, &mux, geometry).map_err(|e| e.to_string())?;
        let sdc = (frame_index == 0).then(|| f.sdc.as_ref().filter(|s| s.crc_ok).map(|s| s.data.clone())).flatten();
        // The services for the status.
        self.ensemble.update_fac(&fac);
        if let Some(data) = &sdc {
            self.ensemble.update_sdc(data);
        }
        self.last_fac = Some(fac);
        Ok(Job::Frame { config, fac, msc, sdc })
    }

    /// Stop the feed thread.
    pub fn stop(&mut self) {
        self.feed_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.feed.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Modulator {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Where the MDI of `settings` comes from (a recording's path against `base_dir`).
pub(crate) fn origin(settings: &MdiSettings, base_dir: Option<&Path>) -> Result<MdiOrigin, String> {
    let mut origin = MdiOrigin::parse(&settings.input).map_err(|e| format!("mdi: input: {e}"))?;
    if let (MdiOrigin::File { path, .. }, Some(dir)) = (&mut origin, base_dir)
        && path.is_relative()
    {
        *path = dir.join(&*path);
        if !path.is_file() {
            return Err(format!("mdi: input: the recording {} does not exist", path.display()));
        }
    }
    Ok(origin)
}

/// A frame the transmission can start with: the first of a super frame (with its SDC),
/// with a valid FAC and the stream layout.
fn starts_super_frame(f: &MdiFrame) -> bool {
    let fac = f.fac_bits().and_then(|b| Fac::parse(&b));
    fac.is_some_and(|fac| fac.channel.frame_index == 0) && f.sdc.as_ref().is_some_and(|s| s.crc_ok) && f.sdci.is_some()
}

/// The feed thread: MDI frames from `input` into `queue`.
fn feed(mut input: MdiInput, queue: SyncSender<Feed>, stop: Arc<AtomicBool>, link: Arc<Mutex<Link>>) {
    let is_file = input.is_file();
    let update = |input: &MdiInput| {
        if let Ok(mut l) = link.lock() {
            l.stats = input.receiver.stats;
            l.sender = input.last_sender.map(|a| a.to_string());
        }
    };
    while !stop.load(Ordering::Relaxed) {
        let read = input.read(WAIT);
        update(&input);
        match read {
            Ok(MdiRead::Frame(f)) => {
                let mut item = Feed::Frame(f);
                loop {
                    match queue.try_send(item) {
                        Ok(()) => break,
                        // A recording waits for the transmission; live MDI is dropped
                        // (the modulator counts the gap).
                        Err(TrySendError::Full(back)) if is_file => {
                            if stop.load(Ordering::Relaxed) {
                                return;
                            }
                            item = back;
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(TrySendError::Full(_)) => break,
                        Err(TrySendError::Disconnected(_)) => return,
                    }
                }
            }
            Ok(MdiRead::Idle) => {}
            Ok(MdiRead::End) => {
                let _ = queue.send(Feed::End);
                return;
            }
            Err(e) => {
                let _ = queue.send(Feed::Error(format!("MDI input: {e}")));
                return;
            }
        }
    }
}

/// A one-line description of a service's audio, e.g. "AAC + SBR, mono, 24 kHz core".
pub(crate) fn describe_audio(p: &decdrm_core::mux::service::AudioParams) -> String {
    let codec = match p.codec {
        AudioCodec::Aac if p.sbr => "HE-AAC",
        AudioCodec::Aac => "AAC",
        AudioCodec::Opus => "Opus",
        AudioCodec::XheAac => "xHE-AAC",
        AudioCodec::Encodec => "EnCodec",
        AudioCodec::Reserved => "audio",
    };
    let mode = match p.mode {
        AudioMode::Mono => "mono",
        AudioMode::ParametricStereo => "parametric stereo",
        AudioMode::Stereo => "stereo",
        AudioMode::Reserved => "reserved mode",
    };
    format!("{codec} {mode}, {} kHz", f64::from(p.sample_rate_hz) / 1000.0)
}
