//! A receiving session without threads or devices: samples in, decoded things out.
//! The engine runs one inside its worker thread; tests and batch tools can use it
//! directly.
//!
//! Pipeline: [`Receiver`] → FAC/SDC into the multiplex model ([`Ensemble`]), which
//! yields the MSC configuration → decoded MSC frames are demultiplexed into streams →
//! the selected audio service's stream goes through the super-frame deframer and the
//! codec (FDK-AAC / Opus / DAC with the `dac` feature), its text message through
//! the text decoder, and every data application's stream through a `decdrm-data`
//! decoder. EVS audio sent in a data application (KCBS, see `decdrm_evs::kcbs`) is
//! recognised there and shown as audio, but not decoded.
//!
//! MDI or RSCI input ([`Session::new_mdi`], [`Session::push_mdi`]) skips the receiver:
//! each frame brings the FAC, the SDC and the MSC streams as decoded elsewhere, and an
//! RSCI receiver's status items stand in for the receiver's status and plots.

use decdrm_codecs::{DrmAudioCoding, DrmAudioDecoder, PcmFrame, open_decoder};
use decdrm_core::fac::{Fac, LANGUAGES, PROGRAMME_TYPES};
use decdrm_core::params::RobustnessMode;
use decdrm_core::mux::audio::AudioDeframer;
use decdrm_core::mux::sdc::{ApplicationInfo, StreamLengths};
use decdrm_core::mux::service::{AudioCodec, AudioMode, AudioParams, Changes, Ensemble, ServiceInfo};
use decdrm_core::mux::text::{TextEvent, TextMessageDecoder};
use decdrm_core::mux::msc::{LogicalFrame, stream_positions};
use decdrm_core::mux::demultiplex;
use decdrm_core::rx::{
    DiversityReceiver, MscConfig, MscFrame, PdsAxis, Receiver, ReceiverConfig, ReceiverEvent, RxState, RxStatus,
    SdcBlock, Visuals,
};
use decdrm_mdi::MdiFrame;
use decdrm_data::datagroup::DataGroup;
use decdrm_data::{AppDomain, DataDecoder, DataEvent, DataServiceConfig, UserApplication};
use decdrm_evs::signalling::Bandwidth as EvsBandwidth;

/// Output of a session step.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Something worth a log line.
    Log(String),
    /// Decoded audio of the selected service (interleaved, 1 or 2 channels).
    Audio(PcmFrame),
    /// A new or changed text message of the selected audio service (`None` = clear).
    Text(Option<String>),
    /// Output of a data application.
    Data { short_id: u8, event: DataEvent },
    /// The service list or its descriptions changed.
    ServicesChanged,
}

/// Multiplex frames decoded, judged by the CRCs of their contents: the frames of
/// the decoded audio service and the packets of every data application.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MscStats {
    /// Multiplex frames decoded (including those while the interleaver fills).
    pub frames: u64,
    /// Frames whose checked contents were all correct.
    pub ok: u64,
    /// Frames with at least one failed check.
    pub bad: u64,
}

/// Audio decoding statistics of the current service.
#[derive(Debug, Clone, Default)]
pub struct AudioStats {
    pub codec: String,
    pub frames_ok: u64,
    pub frames_concealed: u64,
    pub super_frame_errors: u64,
}

struct AudioPipeline {
    short_id: u8,
    stream_id: u8,
    params: AudioParams,
    stream: StreamLengths,
    deframer: AudioDeframer,
    decoder: Box<dyn DrmAudioDecoder>,
    text: TextMessageDecoder,
}

struct DataPipeline {
    short_id: u8,
    app: ApplicationInfo,
    decoder: DataDecoder,
}

/// The service-bar caveat for EVS audio in the KCBS framing: its frames follow EVS in
/// structure, but part of them is nonstandard, most likely selectively encrypted (see
/// [`decdrm_evs::kcbs`]). DecDRM does not decode EVS either way.
const EVS_WARNING: &str = "likely encrypted";

/// A data channel whose data groups carry EVS audio in the KCBS framing
/// ([`decdrm_evs::kcbs`]): how many data groups in a row matched, and the audio
/// bandwidth their frames signal.
#[derive(Debug, Clone)]
struct EvsChannel {
    short_id: u8,
    app: ApplicationInfo,
    matches: u32,
    bandwidth: EvsBandwidth,
}

impl EvsChannel {
    /// Matching data groups in a row before a channel counts as EVS audio.
    const LOCK: u32 = 2;

    fn locked(&self) -> bool {
        self.matches >= Self::LOCK
    }

    /// E.g. `EVS 13.2 kbit/s SWB`.
    fn describe(&self) -> String {
        format!("EVS 13.2 kbit/s {}", self.bandwidth.name())
    }
}

/// The receiver of a session: one, or two combined (diversity reception, see
/// `decdrm_core::rx::diversity`), or none (MDI/RSCI input).
enum Rx {
    Single(Box<Receiver>),
    Diversity(Box<DiversityReceiver>),
    Mdi(Box<MdiRx>),
}

impl Rx {
    fn push(&mut self, branch: usize, frames: &[f32]) -> Vec<ReceiverEvent> {
        match self {
            Rx::Single(r) => r.push(frames),
            Rx::Diversity(d) => d.push(branch, frames),
            Rx::Mdi(_) => Vec::new(),
        }
    }

    fn restart(&mut self) {
        match self {
            Rx::Single(r) => r.restart(),
            Rx::Diversity(d) => d.restart(),
            Rx::Mdi(m) => **m = MdiRx::default(),
        }
    }

    fn set_msc_config(&mut self, cfg: Option<MscConfig>) {
        match self {
            Rx::Single(r) => r.set_msc_config(cfg),
            Rx::Diversity(d) => d.set_msc_config(cfg),
            // The streams come demultiplexed.
            Rx::Mdi(_) => {}
        }
    }

    fn status(&self, branch: usize) -> &RxStatus {
        match self {
            Rx::Single(r) => r.status(),
            Rx::Diversity(d) => d.branch(branch).status(),
            Rx::Mdi(m) => &m.status,
        }
    }

    /// Input channels per sample frame (MDI: none).
    fn channels(&self, branch: usize) -> usize {
        match self {
            Rx::Single(r) => r.config().channels.max(1),
            Rx::Diversity(d) => d.branch(branch).config().channels.max(1),
            Rx::Mdi(_) => 1,
        }
    }

    /// The branch the status and plots show: the one with the better SNR.
    fn shown(&self) -> usize {
        match self {
            Rx::Single(_) | Rx::Mdi(_) => 0,
            Rx::Diversity(d) => {
                let snr = |b: usize| d.branch(b).status().snr_db.unwrap_or(f64::NEG_INFINITY);
                usize::from(snr(1) > snr(0))
            }
        }
    }

    /// Whether no branch but `branch` (which just lost it) is synchronised.
    fn all_lost(&self, branch: usize) -> bool {
        match self {
            Rx::Single(_) | Rx::Mdi(_) => true,
            Rx::Diversity(d) => d.branch(1 - branch.min(1)).status().state == RxState::Acquisition,
        }
    }
}

/// RSCI's power spectral density: the first value lies 7.875 kHz below the DRM
/// signal's DC carrier, then one value per 187.5 Hz (TS 102 349; Dream's plot).
const RSCI_PSD_START_HZ: f64 = -7875.0;
const RSCI_PSD_STEP_HZ: f64 = 187.5;

/// What stands in for the receiver with MDI/RSCI input: a status and plots made from
/// the frames' items (the robustness mode, the FAC and SDC CRCs, an RSCI receiver's
/// status), the spectrum and impulse response of an RSCI receiver.
#[derive(Default)]
struct MdiRx {
    status: RxStatus,
    visuals: Visuals,
}

impl MdiRx {
    fn update(&mut self, f: &MdiFrame, fac_ok: Option<bool>) {
        let st = &mut self.status;
        if let Some(m) = f.robustness {
            st.mode = Some(RobustnessMode::ALL[usize::from(m.min(3))]);
        }
        let r = &f.rsci;
        st.state = match (r.flags, fac_ok) {
            (Some(flags), _) if flags.sync != 0 => RxState::Acquisition,
            (_, Some(true)) => RxState::Locked,
            (Some(_), _) => RxState::Tracking,
            (None, Some(false)) => RxState::Tracking,
            (None, None) => st.state,
        };
        if f.is_rsci() {
            st.mer_db = r.mer_db;
            st.wmer_db = r.wmer_msc_db;
            st.fac_mer_db = r.wmer_fac_db;
            st.doppler_hz = r.doppler_hz.unwrap_or(0.0);
            // The narrowest window holding at least 95 % of the energy, else the widest.
            st.delay_ms = r
                .delay
                .iter()
                .filter(|(p, _)| *p >= 95)
                .map(|&(_, ms)| ms)
                .reduce(f64::min)
                .or_else(|| r.delay.iter().map(|&(_, ms)| ms).reduce(f64::max))
                .unwrap_or(0.0);
        }
        let v = &mut self.visuals;
        if let Some(psd) = &r.psd_db {
            let n = psd.len() as f64;
            v.spectrum_db = psd.clone();
            // Bins from centre − span/2 in steps of span/n: the DC carrier at 0 Hz.
            v.spectrum_span_hz = n * RSCI_PSD_STEP_HZ;
            v.spectrum_centre_hz = RSCI_PSD_START_HZ + v.spectrum_span_hz / 2.0;
            v.real_input = false;
            v.dc_hz = Some(0.0);
            v.signal_band_hz = st.mode.zip(st.occupancy).and_then(|(m, so)| {
                decdrm_core::params::carrier_range(m, so)
                    .map(|(a, b)| (f64::from(a) * m.carrier_spacing(), f64::from(b) * m.carrier_spacing()))
            });
            v.waterfall_rows.push(psd.iter().map(|&x| x as f32).collect());
            let keep = decdrm_core::rx::WATERFALL_ROWS_KEPT;
            if v.waterfall_rows.len() > keep {
                v.waterfall_rows.drain(..v.waterfall_rows.len() - keep);
            }
            v.spectrum_seq += 1;
            v.spectrum_row_s = 0.4;
        }
        if let Some(ir) = &r.impulse_response
            && ir.db.len() >= 2
        {
            v.chain.pds = ir.db.iter().map(|db| 10f64.powf(db / 10.0)).collect();
            v.chain.pds_axis = Some(PdsAxis {
                start_ms: ir.start_ms,
                step_ms: (ir.end_ms - ir.start_ms) / (ir.db.len() - 1) as f64,
                guard_ms: (f64::NAN, f64::NAN),
                pds_begin_ms: f64::NAN,
                pds_end_ms: f64::NAN,
            });
        }
    }
}

/// Receiver plus the service decoding pipelines.
pub struct Session {
    rx: Rx,
    ens: Ensemble,
    msc_config: Option<MscConfig>,
    selected: Option<u8>,
    audio: Option<AudioPipeline>,
    data: Vec<DataPipeline>,
    /// Data channels recognised as carrying EVS audio (or on the way to it).
    evs: Vec<EvsChannel>,
    pub audio_stats: AudioStats,
    pub msc_stats: MscStats,
    text: Option<String>,
    samples_in: u64,
    /// `samples_in` when the last multiplex frame was decoded.
    last_msc_at: Option<u64>,
    last_channel: Option<decdrm_core::fac::ChannelParams>,
}

/// Diversity reception shows the combined cells of the last multiplex frame for this
/// long (three frames), then, while none is decoded, the branch's cells as they come.
const COMBINED_CELLS_SHOWN: u64 = 3 * 19_200;

impl Session {
    pub fn new(cfg: ReceiverConfig) -> Self {
        Self::with(Rx::Single(Box::new(Receiver::new(cfg))))
    }

    /// Diversity reception from two inputs (branches 0 and 1, see
    /// [`Self::push_branch`]).
    pub fn new_diversity(cfg: [ReceiverConfig; 2]) -> Self {
        Self::with(Rx::Diversity(Box::new(DiversityReceiver::new(cfg))))
    }

    /// MDI or RSCI input: frames of the multiplex as decoded elsewhere (see
    /// [`Self::push_mdi`]).
    pub fn new_mdi() -> Self {
        Self::with(Rx::Mdi(Box::default()))
    }

    fn with(rx: Rx) -> Self {
        Self {
            rx,
            ens: Ensemble::new(),
            msc_config: None,
            selected: None,
            audio: None,
            data: Vec::new(),
            evs: Vec::new(),
            audio_stats: AudioStats::default(),
            msc_stats: MscStats::default(),
            text: None,
            samples_in: 0,
            last_msc_at: None,
            last_channel: None,
        }
    }

    /// The receiver's status; in diversity reception the branch with the better SNR;
    /// with MDI/RSCI input what its items say.
    pub fn status(&self) -> &RxStatus {
        self.rx.status(self.rx.shown())
    }

    /// Plot data; in diversity reception the branch with the better SNR, with the MSC
    /// constellation of the combined cells (while multiplex frames are decoded); with
    /// RSCI input the receiver's spectrum and impulse response.
    pub fn visuals(&mut self) -> Visuals {
        let shown = self.rx.shown();
        let combined = self.last_msc_at.is_some_and(|t| self.samples_in.saturating_sub(t) <= COMBINED_CELLS_SHOWN);
        match &mut self.rx {
            Rx::Single(r) => r.visuals(),
            Rx::Mdi(m) => m.visuals.clone(),
            Rx::Diversity(d) => {
                let mut v = d.branch_mut(shown).visuals();
                let cells = d.last_cells();
                if combined && !cells.is_empty() {
                    v.chain.msc = cells.to_vec();
                }
                v
            }
        }
    }

    /// Channels of branch `branch`'s input frames: 1 for a real signal, 2 for I/Q.
    pub fn input_channels(&self, branch: usize) -> usize {
        self.rx.channels(branch)
    }

    /// Diversity reception: the combiner's counts and how it mixes the branches, each
    /// branch's status and constellation.
    pub fn diversity(&self) -> Option<crate::snapshot::DiversityView> {
        match &self.rx {
            Rx::Single(_) | Rx::Mdi(_) => None,
            Rx::Diversity(d) => {
                let (a, b) = (d.branch(0), d.branch(1));
                let mode = a.status().mode.or(b.status().mode);
                Some(crate::snapshot::DiversityView {
                    stats: d.stats(),
                    branches: [a.status().clone(), b.status().clone()],
                    recent: d.recent_mix().clone(),
                    carriers: d.carrier_mix().cloned(),
                    spacing_hz: mode.map_or(0.0, |m| m.carrier_spacing()),
                    msc: [a.msc_cells(), b.msc_cells()],
                })
            }
        }
    }

    pub fn ensemble(&self) -> &Ensemble {
        &self.ens
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Short id of the audio service being decoded (EVS audio of a data service
    /// counts).
    pub fn current_audio_service(&self) -> Option<u8> {
        self.audio.as_ref().map(|a| a.short_id)
    }

    /// The service a UI shows as selected: the audio service being decoded, else the
    /// chosen service if it exists, else the first service with data applications.
    pub fn selected_service(&self) -> Option<u8> {
        self.current_audio_service()
            .or_else(|| self.selected.filter(|&id| self.ens.service(id).is_some()))
            .or_else(|| {
                self.ens.services().find(|s| s.is_data() || !s.applications.is_empty()).map(|s| s.short_id)
            })
    }

    /// Choose the audio service to decode (short id 0..=3).
    pub fn select_service(&mut self, short_id: u8) {
        self.selected = Some(short_id);
        self.rebuild_pipelines(&mut Vec::new());
    }

    pub fn restart(&mut self) {
        self.rx.restart();
        self.reset_multiplex();
    }

    /// Start afresh on another station (a retuned input): [`Self::restart`], and what
    /// belonged to the old one goes too: the service chosen there and the frame counts.
    pub fn new_station(&mut self) {
        self.restart();
        self.selected = None;
        self.audio_stats = AudioStats::default();
        self.msc_stats = MscStats::default();
    }

    fn reset_multiplex(&mut self) {
        self.ens.reset();
        self.msc_config = None;
        self.rx.set_msc_config(None);
        self.audio = None;
        self.data.clear();
        self.evs.clear();
        self.text = None;
        self.last_channel = None;
    }

    /// Seconds of 48 kHz input processed.
    pub fn time_s(&self) -> f64 {
        self.samples_in as f64 / 48_000.0
    }

    /// Feed interleaved 48 kHz frames (diversity reception: of branch 0).
    pub fn push(&mut self, frames: &[f32]) -> Vec<SessionEvent> {
        self.push_branch(0, frames)
    }

    /// Feed interleaved 48 kHz frames of input `branch` (0 or 1; one receiver: 0).
    pub fn push_branch(&mut self, branch: usize, frames: &[f32]) -> Vec<SessionEvent> {
        let ch = self.rx.channels(branch);
        if branch == 0 {
            self.samples_in += (frames.len() / ch) as u64;
        }
        let events = self.rx.push(branch, frames);
        self.handle(branch, events)
    }

    /// The input ended: decode what diversity reception still holds back.
    pub fn flush(&mut self) -> Vec<SessionEvent> {
        let events = match &mut self.rx {
            Rx::Diversity(d) => d.flush(),
            Rx::Single(_) | Rx::Mdi(_) => Vec::new(),
        };
        self.handle(0, events)
    }

    /// One MDI or RSCI frame (a session made with [`Self::new_mdi`]): its FAC and SDC
    /// go into the multiplex model as the receiver's would, its streams straight to the
    /// decoders, its RSCI items into the status. Time advances by a frame (400 ms).
    pub fn push_mdi(&mut self, f: &MdiFrame) -> Vec<SessionEvent> {
        self.samples_in += 19_200;
        let mut events = Vec::new();
        let fac = f.fac_bits().map(|bits| Fac::parse(&bits));
        if let Some(parsed) = &fac {
            match parsed {
                Some(fac) => events.push(ReceiverEvent::Fac(*fac)),
                None => events.push(ReceiverEvent::FacError),
            }
        }
        if let Some(s) = &f.sdc {
            events.push(ReceiverEvent::Sdc(SdcBlock { afs_index: s.afs_index, data: s.data.clone(), crc_ok: s.crc_ok }));
        }
        if let Rx::Mdi(m) = &mut self.rx {
            let st = &mut m.status;
            match &fac {
                Some(Some(fac)) => {
                    st.fac_ok += 1;
                    st.occupancy = Some(fac.channel.occupancy);
                }
                Some(None) => st.fac_bad += 1,
                None => {}
            }
            if let Some(s) = &f.sdc {
                if s.crc_ok {
                    st.sdc_ok += 1;
                } else {
                    st.sdc_bad += 1;
                }
            }
            m.update(f, fac.as_ref().map(Option::is_some));
        }
        let mut out = self.handle(0, events);
        let Some(mux) = self.ens.multiplex() else { return out };
        let hierarchical = self.ens.channel().is_some_and(|c| c.msc_mode.is_hierarchical());
        let mut logical: Vec<Option<LogicalFrame>> = vec![None; mux.streams.len()];
        for p in stream_positions(mux, hierarchical) {
            if let (Some(slot), Some(Some(data))) = (logical.get_mut(usize::from(p.stream_id)), f.streams.get(usize::from(p.stream_id))) {
                *slot = Some(LogicalFrame { stream_id: p.stream_id, data: data.clone(), part_a_len: p.len_a / 8, hierarchical: p.hierarchical });
            }
        }
        self.on_logical(&logical, true, &mut out);
        out
    }

    fn handle(&mut self, branch: usize, events: Vec<ReceiverEvent>) -> Vec<SessionEvent> {
        let t = self.time_s();
        // Diversity reception names the branch an event came from.
        let who = match self.rx {
            Rx::Diversity(_) => format!("branch {}: ", branch + 1),
            Rx::Single(_) | Rx::Mdi(_) => String::new(),
        };
        let mut out = Vec::new();
        for ev in events {
            match ev {
                ReceiverEvent::SignalFound { dc_hz, inverted } => out.push(SessionEvent::Log(format!(
                    "{t:7.2}s {who}signal found at {dc_hz:.1} Hz{}",
                    if inverted { " (inverted spectrum)" } else { "" }
                ))),
                ReceiverEvent::ModeDetected(m) => out.push(SessionEvent::Log(format!("{t:7.2}s {who}robustness mode {m}"))),
                ReceiverEvent::Restarted => {
                    out.push(SessionEvent::Log(format!("{t:7.2}s {who}synchronisation lost, restarting")));
                    // In diversity reception the other branch may carry on.
                    if self.rx.all_lost(branch) {
                        self.reset_multiplex();
                        out.push(SessionEvent::ServicesChanged);
                    }
                }
                // The multiplex is unchanged: keep the services and pipelines.
                ReceiverEvent::Resynchronising => {
                    out.push(SessionEvent::Log(format!("{t:7.2}s {who}timing jump, resynchronising")));
                }
                ReceiverEvent::FrameIdentity { trusted } => out.push(SessionEvent::Log(if trusted {
                    format!("{t:7.2}s {who}the FAC identity counts through the super frame again")
                } else {
                    format!(
                        "{t:7.2}s {who}the FAC identity does not count through the super frame (non-standard \
                         transmitter): the SDC shows where it starts"
                    )
                })),
                ReceiverEvent::Fac(fac) => {
                    self.log_channel_change(&fac, t, &mut out);
                    let changes = self.ens.update_fac(&fac);
                    self.apply_changes(changes, t, &mut out);
                }
                ReceiverEvent::FacError => {}
                ReceiverEvent::Sdc(b) => {
                    if b.crc_ok {
                        let changes = self.ens.update_sdc(&b.data);
                        self.apply_changes(changes, t, &mut out);
                    }
                }
                ReceiverEvent::Msc(frame) => {
                    self.last_msc_at = Some(self.samples_in);
                    self.on_msc(&frame, &mut out);
                }
                // Diversity branches' cells: their receiver combines them.
                ReceiverEvent::MscCells(_) => {}
            }
        }
        out
    }

    fn log_channel_change(&mut self, fac: &Fac, t: f64, out: &mut Vec<SessionEvent>) {
        let c = fac.channel;
        let changed = self.last_channel.is_none_or(|p| {
            p.occupancy != c.occupancy
                || p.msc_mode != c.msc_mode
                || p.sdc_mode != c.sdc_mode
                || p.interleaving != c.interleaving
                || p.num_audio != c.num_audio
                || p.num_data != c.num_data
        });
        if changed {
            out.push(SessionEvent::Log(format!(
                "{t:7.2}s {} · MSC {:?} · SDC {:?} · {:?} interleaving · {} audio / {} data services",
                c.occupancy, c.msc_mode, c.sdc_mode, c.interleaving, c.num_audio, c.num_data
            )));
        }
        self.last_channel = Some(c);
    }

    fn apply_changes(&mut self, changes: Changes, t: f64, out: &mut Vec<SessionEvent>) {
        if !changes.any() {
            return;
        }
        let cfg = self.ens.msc_config();
        if cfg != self.msc_config {
            self.msc_config = cfg;
            self.rx.set_msc_config(cfg);
            if let Some(c) = cfg {
                out.push(SessionEvent::Log(format!(
                    "{t:7.2}s MSC: {:?}, protection A {} B {}, part A {} bytes",
                    c.mode, c.protection.part_a, c.protection.part_b, c.part_a_bytes
                )));
            }
        }
        if changes.labels != 0 {
            for s in self.ens.services() {
                if changes.labels & (1 << s.short_id) != 0
                    && let Some(l) = &s.label
                {
                    out.push(SessionEvent::Log(format!("{t:7.2}s service {} label \"{l}\"", s.short_id)));
                }
            }
        }
        if changes.multiplex || changes.audio != 0 || changes.data != 0 || changes.services != 0 || changes.reconfigured {
            self.rebuild_pipelines(out);
        }
        out.push(SessionEvent::ServicesChanged);
    }

    /// (Re)create the audio pipeline for the selected service and a data pipeline for
    /// every application, keeping those whose parameters did not change.
    fn rebuild_pipelines(&mut self, out: &mut Vec<SessionEvent>) {
        let streams = self.ens.stream_lengths();

        // Audio: the selected service if it is an audio service, else the first one —
        // unless the selected service carries EVS audio in a data application.
        let selected_evs = self.selected.is_some_and(|id| self.evs.iter().any(|c| c.short_id == id && c.locked()));
        let pick = self
            .selected
            .and_then(|id| self.ens.service(id))
            .filter(|s| s.audio.is_some())
            .or_else(|| self.ens.services().find(|s| s.audio.is_some()))
            .filter(|_| !selected_evs)
            .map(|s| (s.short_id, s.audio.clone().expect("filtered")));
        match pick {
            Some((short_id, params)) => {
                let stream = streams.get(params.stream_id as usize).copied();
                let same = self.audio.as_ref().is_some_and(|a| {
                    a.short_id == short_id && a.params == params && Some(a.stream) == stream
                });
                if !same {
                    self.audio = None;
                    if let Some(stream) = stream {
                        match build_audio(short_id, &params, stream) {
                            Ok(p) => {
                                self.audio_stats =
                                    AudioStats { codec: p.decoder.describe(), ..AudioStats::default() };
                                out.push(SessionEvent::Log(format!(
                                    "audio service {short_id}: {} ({})",
                                    self.audio_stats.codec,
                                    describe_audio(&params)
                                )));
                                self.audio = Some(p);
                                self.text = None;
                            }
                            Err(e) => out.push(SessionEvent::Log(format!("audio service {short_id}: {e}"))),
                        }
                    }
                }
            }
            None => self.audio = None,
        }

        // Data applications of every service. An audio service and a data service
        // often both list the same application (e.g. one slideshow), so each
        // (stream, packet id) is decoded once, attributed to a data service if one
        // lists it (services are visited data services first).
        let mut services: Vec<&ServiceInfo> = self.ens.services().collect();
        services.sort_by_key(|s| (!s.is_data(), s.short_id));
        let mut wanted: Vec<(u8, ApplicationInfo)> = Vec::new();
        for s in services {
            for a in &s.applications {
                if !wanted.iter().any(|(_, w)| same_data_channel(w, a)) {
                    wanted.push((s.short_id, a.clone()));
                }
            }
        }
        let mut kept = Vec::new();
        for (short_id, app) in wanted {
            if let Some(pos) = self.data.iter().position(|d| d.short_id == short_id && d.app == app) {
                kept.push(self.data.swap_remove(pos));
                continue;
            }
            let cfg = data_config(&app);
            out.push(SessionEvent::Log(format!(
                "data application on service {short_id}: {:?} (stream {}, {} mode)",
                cfg.application(),
                app.stream_id,
                if app.packet_mode { "packet" } else { "stream" }
            )));
            kept.push(DataPipeline { short_id, app, decoder: DataDecoder::new(cfg) });
        }
        self.data = kept;
        let data = &self.data;
        self.evs.retain(|c| data.iter().any(|d| d.short_id == c.short_id && same_data_channel(&d.app, &c.app)));
    }

    fn on_msc(&mut self, frame: &MscFrame, out: &mut Vec<SessionEvent>) {
        let Some(mux) = self.ens.multiplex() else { return };
        let logical: Vec<Option<LogicalFrame>> = demultiplex(frame, mux);
        self.on_logical(&logical, frame.complete, out);
    }

    /// The streams of one multiplex frame (indexed by stream id) to the decoders.
    /// `complete`: false while the receiver's long interleaver still fills (the
    /// content is unreliable then).
    fn on_logical(&mut self, logical: &[Option<LogicalFrame>], complete: bool, out: &mut Vec<SessionEvent>) {
        // Content checks of this multiplex frame (passed, failed).
        let (mut good, mut bad) = (0u64, 0u64);

        if let Some(a) = self.audio.as_mut()
            && let Some(Some(lf)) = logical.get(a.stream_id as usize)
        {
            let sf = a.deframer.push_frame(lf);
            // Mute while the long interleaver is still filling: CRC-8 alone lets the
            // occasional garbage frame through.
            if complete {
                if let Some(piece) = sf.text {
                    match a.text.push(piece) {
                        Some(TextEvent::Message(m)) => {
                            let txt = m.display_text();
                            if self.text.as_deref() != Some(txt.as_str()) {
                                self.text = Some(txt.clone());
                                out.push(SessionEvent::Text(Some(txt)));
                            }
                        }
                        Some(TextEvent::Clear) => {
                            self.text = None;
                            out.push(SessionEvent::Text(None));
                        }
                        _ => {}
                    }
                }
                if sf.error.is_some() {
                    bad += 1;
                    self.audio_stats.super_frame_errors += 1;
                    for _ in 0..sf.nominal_frames.unwrap_or(0) {
                        if let Ok(pcm) = a.decoder.conceal() {
                            self.audio_stats.frames_concealed += 1;
                            out.push(SessionEvent::Audio(pcm));
                        }
                    }
                }
                for f in &sf.frames {
                    // An xHE-AAC frame whose CRC-16 fails is concealed (§5.3.3), not
                    // decoded: FDK-AAC does not check that CRC, and decoding a corrupt frame
                    // can give a burst far louder than the programme (seen on 6030 kHz: up
                    // to 19 dB above it).
                    let result = if a.params.codec == AudioCodec::XheAac && f.crc_ok == Some(false) {
                        a.decoder.conceal().map(|pcm| PcmFrame { concealed: true, ..pcm })
                    } else {
                        a.decoder.decode(&f.data, f.crc_byte)
                    };
                    match result {
                        Ok(pcm) => {
                            if pcm.concealed {
                                bad += 1;
                                self.audio_stats.frames_concealed += 1;
                            } else {
                                good += 1;
                                self.audio_stats.frames_ok += 1;
                            }
                            out.push(SessionEvent::Audio(pcm));
                        }
                        Err(_) => {
                            bad += 1;
                            self.audio_stats.frames_concealed += 1;
                            if let Ok(pcm) = a.decoder.conceal() {
                                out.push(SessionEvent::Audio(pcm));
                            }
                        }
                    }
                }
            }
        }

        // Data groups are also checked for EVS audio in the KCBS framing (recognised,
        // not decoded: they are captured like any other data).
        for d in &mut self.data {
            if let Some(Some(lf)) = logical.get(d.app.stream_id as usize) {
                for event in d.decoder.push_frame_with_hint(&lf.data, complete) {
                    if let DataEvent::Stats(st) = &event
                        && complete
                    {
                        good += u64::from(st.last_frame_packets_ok);
                        bad += u64::from(st.last_frame_packets_bad);
                    }
                    // Rust note: `self.evs` and `self.data` are different fields, so
                    // borrowing one mutably while iterating the other is allowed.
                    if let DataEvent::Raw { data_group, .. } = &event
                        && let Ok(group) = DataGroup::parse(data_group)
                    {
                        observe_evs(&mut self.evs, d.short_id, &d.app, &group.data, out);
                    }
                    out.push(SessionEvent::Data { short_id: d.short_id, event });
                }
            }
        }

        let m = &mut self.msc_stats;
        m.frames += 1;
        if bad > 0 {
            m.bad += 1;
        } else if good > 0 {
            m.ok += 1;
        }
    }

    /// Every service, described for status displays. A data service carrying EVS audio
    /// is described as that audio (not decodable).
    pub fn service_views(&self) -> Vec<crate::snapshot::ServiceView> {
        let lengths = self.ens.stream_lengths();
        self.ens
            .services()
            .map(|s| {
                let mut v = service_view(s, &lengths);
                if v.audio.is_none()
                    && let Some(c) = self.evs.iter().find(|c| c.short_id == s.short_id && c.locked())
                {
                    v.audio = Some(evs_view(c));
                    v.audio_bitrate = stream_bitrate(&lengths, c.app.stream_id);
                    v.decodable = false;
                    v.warning = Some(EVS_WARNING.into());
                }
                v
            })
            .collect()
    }
}

/// Track the data field `field` of data channel `app` of service `short_id` against the
/// KCBS EVS framing; log when a channel locks (is recognised as EVS audio).
fn observe_evs(channels: &mut Vec<EvsChannel>, short_id: u8, app: &ApplicationInfo, field: &[u8], out: &mut Vec<SessionEvent>) {
    let pos = channels.iter().position(|c| c.short_id == short_id && same_data_channel(&c.app, app));
    match (decdrm_evs::kcbs::detect(field), pos) {
        (Some(bandwidth), Some(i)) => {
            let c = &mut channels[i];
            let was = c.locked();
            c.matches = c.matches.saturating_add(1);
            c.bandwidth = bandwidth;
            if !was && c.locked() {
                out.push(SessionEvent::Log(format!(
                    "service {short_id}: data application {:#05X} carries {} audio (KCBS framing), in a nonstandard \
                     form ({EVS_WARNING}); DecDRM recognises EVS but does not decode it",
                    app.user_app_id().unwrap_or(0),
                    c.describe(),
                )));
                out.push(SessionEvent::ServicesChanged);
            }
        }
        (Some(bandwidth), None) => channels.push(EvsChannel { short_id, app: app.clone(), matches: 1, bandwidth }),
        // A locked channel stays EVS through data groups whose frames do not all signal
        // one bandwidth (the encoder may switch).
        (None, Some(i)) if channels[i].locked() => {}
        (None, Some(i)) => channels[i].matches = 0,
        (None, None) => {}
    }
}

/// The service-bar description of EVS audio: mono at the codec's internal rate (not
/// decoded), the bandwidth as detail.
fn evs_view(c: &EvsChannel) -> crate::snapshot::AudioCodingView {
    crate::snapshot::AudioCodingView {
        codec: "EVS".into(),
        sbr: false,
        parametric_stereo: false,
        stereo: false,
        sample_rate_hz: c.bandwidth.sample_rate_hz(),
        output_rate_hz: c.bandwidth.sample_rate_hz(),
        text: false,
        surround_mode: 0,
        detail: Some(c.bandwidth.name().into()),
    }
}

/// Bit rate of a stream of `lengths` (bytes per 400 ms multiplex frame), bit/s.
fn stream_bitrate(lengths: &[StreamLengths], stream: u8) -> Option<f64> {
    lengths.get(usize::from(stream)).map(|l| (l.part_a + l.part_b) as f64 * 8.0 / 0.4)
}

fn service_view(s: &ServiceInfo, lengths: &[StreamLengths]) -> crate::snapshot::ServiceView {
    let audio_stream = s.audio.as_ref().map(|a| a.stream_id);
    let audio_lengths = audio_stream.and_then(|id| lengths.get(usize::from(id)));
    let fac_language = s.fac.filter(|f| f.language != 0).and_then(|_| s.fac_language());
    crate::snapshot::ServiceView {
        short_id: s.short_id,
        service_id: s.service_id().unwrap_or(0),
        label: s.label.clone().unwrap_or_default(),
        is_audio: s.is_audio(),
        description: describe_service(s),
        // "---" is the SDC code for an unspecified language.
        language: fac_language
            .map(str::to_string)
            .or_else(|| s.language_code.clone().filter(|c| !c.is_empty() && c != "---"))
            .unwrap_or_default(),
        audio: s.audio.as_ref().map(audio_view),
        audio_bitrate: audio_stream.and_then(|id| stream_bitrate(lengths, id)),
        audio_part_a_percent: audio_lengths
            .filter(|l| l.part_a + l.part_b > 0)
            .map(|l| 100.0 * l.part_a as f64 / (l.part_a + l.part_b) as f64),
        apps: s
            .applications
            .iter()
            .map(|a| {
                let id = a.user_app_id().unwrap_or(0);
                crate::snapshot::AppView {
                    name: app_name(UserApplication::from_id(AppDomain::from_sdc(a.app_domain), id)),
                    user_app_id: id,
                    stream_id: a.stream_id,
                    packet_mode: a.packet_mode,
                    packet_id: a.packet_id,
                    stream_bitrate: stream_bitrate(lengths, a.stream_id),
                }
            })
            .collect(),
        programme_type: s.fac.filter(|f| !f.is_data && f.descriptor != 0).and_then(|_| s.programme_type()).map(str::to_string),
        country: s.country_code.as_ref().filter(|c| !c.is_empty() && *c != "--").map(|c| c.to_ascii_uppercase()),
        ca: s.fac.is_some_and(|f| f.audio_ca || f.data_ca),
        decodable: s.audio.as_ref().is_some_and(|a| match a.codec {
            AudioCodec::Reserved | AudioCodec::Encodec => false,
            AudioCodec::Dac => decdrm_dac::BUILT_IN,
            _ => true,
        }),
        warning: None,
    }
}

fn audio_view(p: &AudioParams) -> crate::snapshot::AudioCodingView {
    let codec = match p.codec {
        AudioCodec::Aac => "AAC",
        AudioCodec::XheAac => "xHE-AAC",
        AudioCodec::Opus => "Opus",
        AudioCodec::Dac => "DAC",
        AudioCodec::Encodec => "EnCodec",
        AudioCodec::Reserved => "reserved",
    };
    let detail = match p.codec {
        AudioCodec::Dac => Some(
            decdrm_dac::DacConfig::from_codec_config(&p.codec_config).map_or_else(|e| e.to_string(), |c| c.bandwidth.to_string()),
        ),
        AudioCodec::Encodec => Some("DecDRM 0.4.6 and earlier, no longer supported".to_string()),
        _ => None,
    };
    crate::snapshot::AudioCodingView {
        codec: codec.to_string(),
        sbr: p.sbr,
        parametric_stereo: p.mode == AudioMode::ParametricStereo,
        stereo: p.mode == AudioMode::Stereo,
        sample_rate_hz: p.sample_rate_hz,
        output_rate_hz: if p.codec == AudioCodec::Aac && p.sbr { 2 * p.sample_rate_hz } else { p.sample_rate_hz },
        text: p.text_flag,
        surround_mode: p.surround_mode,
        detail,
    }
}

/// Display name of a data application.
fn app_name(app: UserApplication) -> String {
    match app {
        UserApplication::SlideShow => "MOT Slideshow".into(),
        UserApplication::BroadcastWebsite => "Broadcast Website".into(),
        UserApplication::Tpeg => "TPEG".into(),
        UserApplication::Epg => "EPG".into(),
        UserApplication::Journaline => "Journaline".into(),
        UserApplication::Other(id) => format!("application {id:#05X}"),
    }
}

fn build_audio(short_id: u8, params: &AudioParams, stream: StreamLengths) -> Result<AudioPipeline, String> {
    let coding = match params.codec {
        AudioCodec::Aac => Some(DrmAudioCoding::Aac),
        AudioCodec::XheAac => Some(DrmAudioCoding::XheAac),
        AudioCodec::Opus => Some(DrmAudioCoding::Opus),
        AudioCodec::Dac => None,
        AudioCodec::Encodec => return Err(decdrm_dac::ConfigError::Encodec.to_string()),
        AudioCodec::Reserved => return Err("reserved audio coding (CELP/HVXC are not supported)".into()),
    };
    let deframer = AudioDeframer::new(params, stream).map_err(|e| e.to_string())?;
    let decoder = match coding {
        Some(coding) => open_decoder(coding, &params.type9_bytes).map_err(|e| e.to_string())?,
        None => open_dac(params)?,
    };
    Ok(AudioPipeline {
        short_id,
        stream_id: params.stream_id,
        params: params.clone(),
        stream,
        deframer,
        decoder,
        text: TextMessageDecoder::new(),
    })
}

/// The decoder of a DecDRM DAC service (the `dac` feature). The model weights are
/// loaded (once per process) from the default location, see `decdrm_dac::weights`.
#[cfg(feature = "dac")]
fn open_dac(params: &AudioParams) -> Result<Box<dyn DrmAudioDecoder>, String> {
    decdrm_dac::open_decoder(&params.type9_bytes).map_err(|e| e.to_string())
}

#[cfg(not(feature = "dac"))]
fn open_dac(_params: &AudioParams) -> Result<Box<dyn DrmAudioDecoder>, String> {
    Err("DAC (not built in)".into())
}

/// Whether two application entries describe the same data channel: the same stream,
/// and in packet mode the same packet id.
fn same_data_channel(a: &ApplicationInfo, b: &ApplicationInfo) -> bool {
    a.stream_id == b.stream_id && a.packet_mode == b.packet_mode && (!a.packet_mode || a.packet_id == b.packet_id)
}

fn data_config(app: &ApplicationInfo) -> DataServiceConfig {
    DataServiceConfig {
        packet_mode: app.packet_mode,
        data_unit_indicator: app.data_unit_indicator,
        packet_id: app.packet_id,
        packet_len: DataServiceConfig::total_packet_len(app.packet_length),
        app_domain: AppDomain::from_sdc(app.app_domain),
        user_app_id: app.user_app_id().unwrap_or(0),
        app_data: app.user_app_data().to_vec(),
    }
}

fn describe_audio(p: &AudioParams) -> String {
    let codec = match p.codec {
        AudioCodec::Aac if p.sbr => "HE-AAC",
        AudioCodec::Aac => "AAC",
        AudioCodec::XheAac => "xHE-AAC",
        AudioCodec::Opus => "Opus",
        AudioCodec::Dac => return describe_dac(p),
        AudioCodec::Encodec => return "EnCodec (DecDRM 0.4.6 and earlier, no longer supported)".to_string(),
        AudioCodec::Reserved => "reserved",
    };
    let mode = match p.mode {
        AudioMode::Mono => "mono",
        AudioMode::ParametricStereo => "parametric stereo",
        AudioMode::Stereo => "stereo",
        _ => "?",
    };
    format!("{codec} {mode} {} kHz{}", p.sample_rate_hz / 1000, if p.text_flag { ", text" } else { "" })
}

/// E.g. `DAC 6 kbit/s mono 24 kHz, text` — DecDRM's neural codec extension — with
/// `(not built in)` when this build cannot decode it (no `dac` feature).
fn describe_dac(p: &AudioParams) -> String {
    let bandwidth = match decdrm_dac::DacConfig::from_codec_config(&p.codec_config) {
        Ok(c) => format!("{} ", c.bandwidth),
        Err(e) => format!("({e}) "),
    };
    format!(
        "DAC {bandwidth}mono {} kHz{}{}",
        p.sample_rate_hz / 1000,
        if p.text_flag { ", text" } else { "" },
        if decdrm_dac::BUILT_IN { "" } else { " (not built in)" }
    )
}

fn describe_service(s: &ServiceInfo) -> String {
    if let Some(a) = &s.audio {
        let pty = s.programme_type().unwrap_or("");
        return format!("{} · {pty}", describe_audio(a));
    }
    if let Some(app) = s.applications.first() {
        return format!("data, app {:#06x}", app.user_app_id().unwrap_or(0));
    }
    match &s.fac {
        Some(f) if f.is_data => format!("data service, app {:#x}", f.descriptor),
        Some(f) => format!(
            "audio, {}, {}",
            PROGRAMME_TYPES.get(f.descriptor as usize).copied().unwrap_or("?"),
            LANGUAGES.get(f.language as usize).copied().unwrap_or("?")
        ),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::fac::ServiceParams;

    fn fac(short_id: u8, is_data: bool, descriptor: u8) -> ServiceParams {
        ServiceParams { service_id: 0xD0D000 + u32::from(short_id), short_id, audio_ca: false, language: 5, is_data, descriptor, data_ca: false }
    }

    fn app(stream_id: u8, packet_id: u8, user_app: u16) -> ApplicationInfo {
        ApplicationInfo {
            short_id: 0,
            stream_id,
            packet_mode: true,
            data_unit_indicator: true,
            packet_id,
            enhancement: false,
            app_domain: 1,
            packet_length: 45,
            application_data: user_app.to_be_bytes().to_vec(),
        }
    }

    /// §5.3.3: an xHE-AAC frame whose CRC-16 fails is concealed, not decoded (FDK-AAC does
    /// not check it). Only the CRC of one frame is damaged, its access unit is intact: FDK
    /// would decode it without complaint, so only the session's check can catch it.
    #[test]
    fn xhe_frame_failing_its_crc_is_concealed() {
        use decdrm_codecs::{XheAacConfig, XheAacEncoder};
        use decdrm_core::mux::audio::XheAacFramer;
        let len = 600; // bytes per 400 ms: 12 kbit/s
        let mut enc = XheAacEncoder::new(XheAacConfig::with_super_frame_bytes(24_000, 1, len)).unwrap();
        let info = enc.audio_info();
        let params = AudioParams {
            stream_id: 0,
            codec: AudioCodec::XheAac,
            sbr: false,
            mode: AudioMode::Mono,
            sample_rate_hz: 24_000,
            text_flag: false,
            enhancement: false,
            surround_mode: 0,
            codec_config: info.xhe_aac_config.clone(),
            type9_bytes: info.to_type9_bytes(),
        };
        let mut session = Session::new(ReceiverConfig::default());
        session.audio = Some(build_audio(0, &params, StreamLengths { part_a: 0, part_b: len }).unwrap());
        let mut framer = XheAacFramer::new();
        let n = enc.frame_len();
        let (mut t, mut damaged, mut events) = (0usize, false, Vec::new());
        for _ in 0..12 {
            while !framer.ready(len) {
                let pcm: Vec<f32> = (t..t + n)
                    .map(|i| (0.3 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 24_000.0).sin()) as f32)
                    .collect();
                t += n;
                for au in enc.encode(&pcm).unwrap() {
                    framer.push_access_unit(&au.data, au.bit_reservoir_level);
                }
            }
            let mut data = framer.next_super_frame(len).unwrap();
            // The directory's last element is border 0; the byte before it is the last CRC
            // byte of the frame that ends there.
            let border = usize::from(u16::from_be_bytes([data[len - 2], data[len - 1]]) >> 4);
            if !damaged && t > 10 * n && (1..0xFFE).contains(&border) {
                data[2 + border - 1] ^= 0x01;
                damaged = true;
            }
            let lf = LogicalFrame { stream_id: 0, data, part_a_len: 0, hierarchical: false };
            session.on_logical(&[Some(lf)], true, &mut events);
        }
        assert!(damaged);
        let concealed = events.iter().filter(|e| matches!(e, SessionEvent::Audio(p) if p.concealed)).count();
        assert_eq!((session.audio_stats.frames_concealed, concealed), (1, 1));
        assert!(session.audio_stats.frames_ok >= 40, "{}", session.audio_stats.frames_ok);
    }

    /// Dream's service-bar facts: codec features, output rate, bit rates from the stream
    /// lengths, UEP share, applications with their stream bit rates.
    #[test]
    fn service_views_for_the_bars() {
        let lengths = [StreamLengths { part_a: 100, part_b: 500 }, StreamLengths { part_a: 0, part_b: 60 }];
        let audio = ServiceInfo {
            short_id: 0,
            fac: Some(fac(0, false, 10)),
            label: Some("Radio".into()),
            language_code: Some("eng".into()),
            country_code: Some("gb".into()),
            audio: Some(AudioParams {
                stream_id: 0,
                codec: AudioCodec::Aac,
                sbr: true,
                mode: AudioMode::ParametricStereo,
                sample_rate_hz: 24_000,
                text_flag: true,
                enhancement: false,
                surround_mode: 0,
                codec_config: Vec::new(),
                type9_bytes: Vec::new(),
            }),
            applications: vec![app(1, 0, 0x002)],
            conditional_access: Vec::new(),
        };
        let v = service_view(&audio, &lengths);
        let a = v.audio.as_ref().unwrap();
        assert_eq!((a.codec.as_str(), a.sbr, a.parametric_stereo, a.stereo), ("AAC", true, true, false));
        assert_eq!((a.sample_rate_hz, a.output_rate_hz, a.text), (24_000, 48_000, true));
        assert_eq!(v.audio_bitrate, Some(12_000.0), "600 bytes per 400 ms");
        assert!((v.audio_part_a_percent.unwrap() - 100.0 / 6.0).abs() < 1e-9, "UEP");
        assert_eq!(v.apps.len(), 1);
        assert_eq!((v.apps[0].name.as_str(), v.apps[0].stream_bitrate), ("MOT Slideshow", Some(1_200.0)));
        assert_eq!(v.language, "English", "the FAC language name before the SDC code");
        assert_eq!(v.country.as_deref(), Some("GB"));
        assert_eq!(v.programme_type.as_deref(), Some("Pop Music"));
        assert!(v.decodable && !v.ca);

        let data = ServiceInfo {
            short_id: 1,
            fac: Some(ServiceParams { data_ca: true, language: 0, ..fac(1, true, 0) }),
            label: Some("News".into()),
            applications: vec![app(1, 1, 0x44A), app(1, 2, 0x123)],
            ..Default::default()
        };
        let v = service_view(&data, &lengths);
        assert!(v.audio.is_none() && v.audio_bitrate.is_none() && !v.decodable && v.ca);
        let names: Vec<&str> = v.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Journaline", "application 0x123"]);
        assert_eq!(v.programme_type, None);
        assert_eq!(v.language, "", "no FAC language and no SDC code");
    }
}
