//! A receiving session without threads or devices: samples in, decoded things out.
//! The engine runs one inside its worker thread; tests and batch tools can use it
//! directly.
//!
//! Pipeline: [`Receiver`] → FAC/SDC into the multiplex model ([`Ensemble`]), which
//! yields the MSC configuration → decoded MSC frames are demultiplexed into streams →
//! the selected audio service's stream goes through the super-frame deframer and the
//! codec (FDK-AAC / Opus / EnCodec with the `encodec` feature), its text message through
//! the text decoder, and every data application's stream through a `decdrm-data`
//! decoder. EVS audio sent in a data application (KCBS, see `decdrm_evs::kcbs`) is
//! recognised there and, with the `evs` feature, decoded like an audio service.

use decdrm_codecs::{DrmAudioCoding, DrmAudioDecoder, PcmFrame, open_decoder};
use decdrm_core::fac::{Fac, LANGUAGES, PROGRAMME_TYPES};
use decdrm_core::mux::audio::AudioDeframer;
use decdrm_core::mux::sdc::{ApplicationInfo, StreamLengths};
use decdrm_core::mux::service::{AudioCodec, AudioMode, AudioParams, Changes, Ensemble, ServiceInfo};
use decdrm_core::mux::text::{TextEvent, TextMessageDecoder};
use decdrm_core::mux::{demultiplex, msc::LogicalFrame};
use decdrm_core::rx::{MscConfig, MscFrame, Receiver, ReceiverConfig, ReceiverEvent, RxStatus, Visuals};
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

/// Decoding the EVS audio of one service (the `evs` feature).
#[cfg_attr(not(feature = "evs"), allow(dead_code))]
struct EvsPlayer {
    short_id: u8,
    /// A data group was decoded (so there is something to conceal from).
    started: bool,
    #[cfg(feature = "evs")]
    decoder: decdrm_evs::EvsDecoder,
}

/// Receiver plus the service decoding pipelines.
pub struct Session {
    rx: Receiver,
    ens: Ensemble,
    msc_config: Option<MscConfig>,
    selected: Option<u8>,
    audio: Option<AudioPipeline>,
    data: Vec<DataPipeline>,
    /// Data channels recognised as carrying EVS audio (or on the way to it).
    evs: Vec<EvsChannel>,
    evs_player: Option<EvsPlayer>,
    pub audio_stats: AudioStats,
    pub msc_stats: MscStats,
    text: Option<String>,
    samples_in: u64,
    last_channel: Option<decdrm_core::fac::ChannelParams>,
}

impl Session {
    pub fn new(cfg: ReceiverConfig) -> Self {
        Self {
            rx: Receiver::new(cfg),
            ens: Ensemble::new(),
            msc_config: None,
            selected: None,
            audio: None,
            data: Vec::new(),
            evs: Vec::new(),
            evs_player: None,
            audio_stats: AudioStats::default(),
            msc_stats: MscStats::default(),
            text: None,
            samples_in: 0,
            last_channel: None,
        }
    }

    pub fn status(&self) -> &RxStatus {
        self.rx.status()
    }

    pub fn visuals(&self) -> Visuals {
        self.rx.visuals()
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
        self.audio.as_ref().map(|a| a.short_id).or_else(|| self.evs_player.as_ref().map(|p| p.short_id))
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

    fn reset_multiplex(&mut self) {
        self.ens.reset();
        self.msc_config = None;
        self.rx.set_msc_config(None);
        self.audio = None;
        self.data.clear();
        self.evs.clear();
        self.evs_player = None;
        self.text = None;
        self.last_channel = None;
    }

    /// Seconds of 48 kHz input processed.
    pub fn time_s(&self) -> f64 {
        self.samples_in as f64 / 48_000.0
    }

    /// Feed interleaved 48 kHz frames.
    pub fn push(&mut self, frames: &[f32]) -> Vec<SessionEvent> {
        let ch = self.rx.config().channels.max(1);
        self.samples_in += (frames.len() / ch) as u64;
        let t = self.time_s();
        let mut out = Vec::new();
        for ev in self.rx.push(frames) {
            match ev {
                ReceiverEvent::SignalFound { dc_hz, inverted } => out.push(SessionEvent::Log(format!(
                    "{t:7.2}s signal found at {dc_hz:.1} Hz{}",
                    if inverted { " (inverted spectrum)" } else { "" }
                ))),
                ReceiverEvent::ModeDetected(m) => out.push(SessionEvent::Log(format!("{t:7.2}s robustness mode {m}"))),
                ReceiverEvent::Restarted => {
                    self.reset_multiplex();
                    out.push(SessionEvent::Log(format!("{t:7.2}s synchronisation lost, restarting")));
                    out.push(SessionEvent::ServicesChanged);
                }
                // The multiplex is unchanged: keep the services and pipelines.
                ReceiverEvent::Resynchronising => {
                    out.push(SessionEvent::Log(format!("{t:7.2}s timing jump, resynchronising")));
                }
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
                ReceiverEvent::Msc(frame) => self.on_msc(&frame, &mut out),
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
        // Content checks of this multiplex frame (passed, failed).
        let (mut good, mut bad) = (0u64, 0u64);

        if let Some(a) = self.audio.as_mut()
            && let Some(Some(lf)) = logical.get(a.stream_id as usize)
        {
            let sf = a.deframer.push_frame(lf);
            // Mute while the long interleaver is still filling: CRC-8 alone lets the
            // occasional garbage frame through.
            if frame.complete {
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
                    match a.decoder.decode(&f.data, f.crc_byte) {
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

        // Data groups of data channels carrying EVS audio, by service.
        let mut evs_fields: Vec<(u8, Vec<u8>)> = Vec::new();
        for d in &mut self.data {
            if let Some(Some(lf)) = logical.get(d.app.stream_id as usize) {
                for event in d.decoder.push_frame_with_hint(&lf.data, frame.complete) {
                    if let DataEvent::Stats(st) = &event
                        && frame.complete
                    {
                        good += u64::from(st.last_frame_packets_ok);
                        bad += u64::from(st.last_frame_packets_bad);
                    }
                    // Rust note: `self.evs` and `self.data` are different fields, so
                    // borrowing one mutably while iterating the other is allowed.
                    if let DataEvent::Raw { data_group, .. } = &event
                        && let Ok(group) = DataGroup::parse(data_group)
                        && observe_evs(&mut self.evs, d.short_id, &d.app, &group.data, out)
                    {
                        evs_fields.push((d.short_id, group.data));
                        // Decoded as audio rather than captured, when it can be.
                        if decdrm_evs::BUILT_IN {
                            continue;
                        }
                    }
                    out.push(SessionEvent::Data { short_id: d.short_id, event });
                }
            }
        }
        self.play_evs(&evs_fields, frame.complete, &mut good, &mut bad, out);

        let m = &mut self.msc_stats;
        m.frames += 1;
        if bad > 0 {
            m.bad += 1;
        } else if good > 0 {
            m.ok += 1;
        }
    }

    /// The service whose EVS audio to decode: none while an ordinary audio service
    /// plays; the selected service if it carries EVS audio; with nothing selected, the
    /// first that does.
    fn evs_target(&self) -> Option<u8> {
        if self.audio.is_some() {
            return None;
        }
        let carries = |id: u8| self.evs.iter().any(|c| c.short_id == id && c.locked());
        match self.selected {
            Some(id) => carries(id).then_some(id),
            None => self.evs.iter().find(|c| c.locked()).map(|c| c.short_id),
        }
    }

    /// Decode the EVS audio of [`Self::evs_target`] from this multiplex frame's data
    /// groups (`fields`: data fields by service), 20 frames each; a complete multiplex
    /// frame without one is concealed.
    fn play_evs(&mut self, fields: &[(u8, Vec<u8>)], complete: bool, good: &mut u64, bad: &mut u64, out: &mut Vec<SessionEvent>) {
        let target = self.evs_target();
        if target != self.evs_player.as_ref().map(|p| p.short_id) {
            self.evs_player = target.and_then(|id| self.open_evs_player(id, out));
        }
        let Some(p) = self.evs_player.as_mut() else { return };
        let mut any = false;
        let short_id = p.short_id;
        for (_, field) in fields.iter().filter(|(id, _)| *id == short_id) {
            let Some(frames) = decdrm_evs::kcbs::frames(field) else { continue };
            any = true;
            p.started = true;
            *good += 1;
            for f in &frames {
                // The frame types KCBS's encoder gets wrong are decoded as lost (EVS
                // concealment); they still count as received.
                let frame = (!decdrm_evs::kcbs::unreliable(f)).then_some(&f[..]);
                match evs_decode(p, frame) {
                    Some(pcm) => {
                        self.audio_stats.frames_ok += 1;
                        out.push(SessionEvent::Audio(pcm));
                    }
                    None => self.audio_stats.frames_concealed += 1,
                }
            }
        }
        if !any && complete && p.started {
            *bad += 1;
            self.audio_stats.super_frame_errors += 1;
            for _ in 0..decdrm_evs::kcbs::FRAMES {
                self.audio_stats.frames_concealed += 1;
                if let Some(pcm) = evs_decode(p, None) {
                    out.push(SessionEvent::Audio(pcm));
                }
            }
        }
    }

    #[cfg(feature = "evs")]
    fn open_evs_player(&mut self, short_id: u8, out: &mut Vec<SessionEvent>) -> Option<EvsPlayer> {
        let channel = self.evs.iter().find(|c| c.short_id == short_id)?;
        match decdrm_evs::EvsDecoder::new(48_000) {
            Ok(decoder) => {
                self.audio_stats = AudioStats { codec: format!("{} (KCBS framing, its faulty frame types concealed)", channel.describe()), ..AudioStats::default() };
                out.push(SessionEvent::Log(format!(
                    "audio service {short_id}: {}, carried in data application {:#05X}",
                    self.audio_stats.codec,
                    channel.app.user_app_id().unwrap_or(0)
                )));
                self.text = None;
                Some(EvsPlayer { short_id, started: false, decoder })
            }
            Err(e) => {
                out.push(SessionEvent::Log(format!("audio service {short_id}: {e}")));
                None
            }
        }
    }

    /// Without the `evs` feature EVS audio is only reported.
    #[cfg(not(feature = "evs"))]
    fn open_evs_player(&mut self, _short_id: u8, _out: &mut Vec<SessionEvent>) -> Option<EvsPlayer> {
        None
    }

    /// Every service, described for status displays. A data service carrying EVS audio
    /// is described as that audio.
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
                    v.decodable = decdrm_evs::BUILT_IN;
                }
                v
            })
            .collect()
    }
}

/// Track the data field `field` of data channel `app` of service `short_id` against the
/// KCBS EVS framing: whether it is EVS audio to decode (the channel locked). Logs when a
/// channel locks.
fn observe_evs(channels: &mut Vec<EvsChannel>, short_id: u8, app: &ApplicationInfo, field: &[u8], out: &mut Vec<SessionEvent>) -> bool {
    let pos = channels.iter().position(|c| c.short_id == short_id && same_data_channel(&c.app, app));
    match (decdrm_evs::kcbs::detect(field), pos) {
        (Some(bandwidth), Some(i)) => {
            let c = &mut channels[i];
            let was = c.locked();
            c.matches = c.matches.saturating_add(1);
            c.bandwidth = bandwidth;
            if !was && c.locked() {
                out.push(SessionEvent::Log(format!(
                    "service {short_id}: data application {:#05X} carries {} audio (KCBS framing){}",
                    app.user_app_id().unwrap_or(0),
                    c.describe(),
                    if decdrm_evs::BUILT_IN { "" } else { "; this build has no EVS decoder (feature `evs`)" }
                )));
                out.push(SessionEvent::ServicesChanged);
            }
            c.locked()
        }
        (Some(bandwidth), None) => {
            channels.push(EvsChannel { short_id, app: app.clone(), matches: 1, bandwidth });
            false
        }
        // A locked channel stays EVS through data groups whose frames do not all signal
        // one bandwidth (the encoder may switch), as long as the frames can be cut out.
        (None, Some(i)) if channels[i].locked() => decdrm_evs::kcbs::frames(field).is_some(),
        (None, Some(i)) => {
            channels[i].matches = 0;
            false
        }
        (None, None) => false,
    }
}

/// The next 20 ms of EVS audio: `frame` decoded, or concealment for `None`.
#[cfg(feature = "evs")]
fn evs_decode(p: &mut EvsPlayer, frame: Option<&[u8]>) -> Option<PcmFrame> {
    let rate = p.decoder.rate();
    p.decoder.decode(frame).ok().map(|samples| PcmFrame { sample_rate: rate, channels: 1, samples, concealed: frame.is_none() })
}

#[cfg(not(feature = "evs"))]
fn evs_decode(_p: &mut EvsPlayer, _frame: Option<&[u8]>) -> Option<PcmFrame> {
    None
}

/// The service-bar description of EVS audio: mono at the codec's internal rate,
/// decoded to 48 kHz, the bandwidth as detail.
fn evs_view(c: &EvsChannel) -> crate::snapshot::AudioCodingView {
    crate::snapshot::AudioCodingView {
        codec: "EVS".into(),
        sbr: false,
        parametric_stereo: false,
        stereo: false,
        sample_rate_hz: c.bandwidth.sample_rate_hz(),
        output_rate_hz: 48_000,
        text: false,
        surround: false,
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
            AudioCodec::Reserved => false,
            AudioCodec::Encodec => decdrm_encodec::BUILT_IN,
            _ => true,
        }),
    }
}

fn audio_view(p: &AudioParams) -> crate::snapshot::AudioCodingView {
    let codec = match p.codec {
        AudioCodec::Aac => "AAC",
        AudioCodec::XheAac => "xHE-AAC",
        AudioCodec::Opus => "Opus",
        AudioCodec::Encodec => "EnCodec",
        AudioCodec::Reserved => "reserved",
    };
    let detail = (p.codec == AudioCodec::Encodec).then(|| {
        decdrm_encodec::EncodecConfig::from_codec_config(&p.codec_config)
            .map_or_else(|e| e.to_string(), |c| c.bandwidth.to_string())
    });
    crate::snapshot::AudioCodingView {
        codec: codec.to_string(),
        sbr: p.sbr,
        parametric_stereo: p.mode == AudioMode::ParametricStereo,
        stereo: p.mode == AudioMode::Stereo,
        sample_rate_hz: p.sample_rate_hz,
        output_rate_hz: if p.codec == AudioCodec::Aac && p.sbr { 2 * p.sample_rate_hz } else { p.sample_rate_hz },
        text: p.text_flag,
        surround: p.surround_mode != 0,
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
        AudioCodec::Encodec => None,
        AudioCodec::Reserved => return Err("reserved audio coding (CELP/HVXC are not supported)".into()),
    };
    let deframer = AudioDeframer::new(params, stream).map_err(|e| e.to_string())?;
    let decoder = match coding {
        Some(coding) => open_decoder(coding, &params.type9_bytes).map_err(|e| e.to_string())?,
        None => open_encodec(params)?,
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

/// The decoder of a DecDRM EnCodec service (the `encodec` feature). The model weights
/// are loaded (once per process) from the default location, see
/// `decdrm_encodec::weights`.
#[cfg(feature = "encodec")]
fn open_encodec(params: &AudioParams) -> Result<Box<dyn DrmAudioDecoder>, String> {
    decdrm_encodec::open_decoder(&params.type9_bytes).map_err(|e| e.to_string())
}

#[cfg(not(feature = "encodec"))]
fn open_encodec(_params: &AudioParams) -> Result<Box<dyn DrmAudioDecoder>, String> {
    Err("EnCodec (not built in)".into())
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
        AudioCodec::Encodec => return describe_encodec(p),
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

/// E.g. `EnCodec 6 kbit/s mono 24 kHz, text` — DecDRM's experimental extension — with
/// `(not built in)` when this build cannot decode it (no `encodec` feature).
fn describe_encodec(p: &AudioParams) -> String {
    let bandwidth = match decdrm_encodec::EncodecConfig::from_codec_config(&p.codec_config) {
        Ok(c) => format!("{} ", c.bandwidth),
        Err(e) => format!("({e}) "),
    };
    format!(
        "EnCodec {bandwidth}mono {} kHz{}{}",
        p.sample_rate_hz / 1000,
        if p.text_flag { ", text" } else { "" },
        if decdrm_encodec::BUILT_IN { "" } else { " (not built in)" }
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
