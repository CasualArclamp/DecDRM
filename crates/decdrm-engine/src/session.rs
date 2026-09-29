//! A receiving session without threads or devices: samples in, decoded things out.
//! The engine runs one inside its worker thread; tests and batch tools can use it
//! directly.
//!
//! Pipeline: [`Receiver`] → FAC/SDC into the multiplex model ([`Ensemble`]), which
//! yields the MSC configuration → decoded MSC frames are demultiplexed into streams →
//! the selected audio service's stream goes through the super-frame deframer and the
//! codec (FDK-AAC / Opus), its text message through the text decoder, and every data
//! application's stream through a `decdrm-data` decoder.

use decdrm_codecs::{DrmAudioCoding, DrmAudioDecoder, PcmFrame, open_decoder};
use decdrm_core::fac::{Fac, LANGUAGES, PROGRAMME_TYPES};
use decdrm_core::mux::audio::AudioDeframer;
use decdrm_core::mux::sdc::{ApplicationInfo, StreamLengths};
use decdrm_core::mux::service::{AudioCodec, AudioMode, AudioParams, Changes, Ensemble, ServiceInfo};
use decdrm_core::mux::text::{TextEvent, TextMessageDecoder};
use decdrm_core::mux::{demultiplex, msc::LogicalFrame};
use decdrm_core::rx::{MscConfig, MscFrame, Receiver, ReceiverConfig, ReceiverEvent, RxStatus, Visuals};
use decdrm_data::{AppDomain, DataDecoder, DataEvent, DataServiceConfig};

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

/// Receiver plus the service decoding pipelines.
pub struct Session {
    rx: Receiver,
    ens: Ensemble,
    msc_config: Option<MscConfig>,
    selected: Option<u8>,
    audio: Option<AudioPipeline>,
    data: Vec<DataPipeline>,
    pub audio_stats: AudioStats,
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
            audio_stats: AudioStats::default(),
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

    /// Short id of the audio service being decoded.
    pub fn current_audio_service(&self) -> Option<u8> {
        self.audio.as_ref().map(|a| a.short_id)
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

        // Audio: the selected service if it is an audio service, else the first one.
        let pick = self
            .selected
            .and_then(|id| self.ens.service(id))
            .filter(|s| s.audio.is_some())
            .or_else(|| self.ens.services().find(|s| s.audio.is_some()))
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
    }

    fn on_msc(&mut self, frame: &MscFrame, out: &mut Vec<SessionEvent>) {
        let Some(mux) = self.ens.multiplex() else { return };
        let logical: Vec<Option<LogicalFrame>> = demultiplex(frame, mux);

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
                                self.audio_stats.frames_concealed += 1;
                            } else {
                                self.audio_stats.frames_ok += 1;
                            }
                            out.push(SessionEvent::Audio(pcm));
                        }
                        Err(_) => {
                            self.audio_stats.frames_concealed += 1;
                            if let Ok(pcm) = a.decoder.conceal() {
                                out.push(SessionEvent::Audio(pcm));
                            }
                        }
                    }
                }
            }
        }

        for d in &mut self.data {
            if let Some(Some(lf)) = logical.get(d.app.stream_id as usize) {
                for event in d.decoder.push_frame_with_hint(&lf.data, frame.complete) {
                    if !matches!(event, DataEvent::Stats(_)) {
                        out.push(SessionEvent::Data { short_id: d.short_id, event });
                    }
                }
            }
        }
    }

    /// Human-readable description of every service, for status displays.
    pub fn service_views(&self) -> Vec<crate::snapshot::ServiceView> {
        self.ens
            .services()
            .map(|s| crate::snapshot::ServiceView {
                short_id: s.short_id,
                service_id: s.service_id().unwrap_or(0),
                label: s.label.clone().unwrap_or_default(),
                is_audio: s.is_audio(),
                description: describe_service(s),
                language: s
                    .language_code
                    .clone()
                    .or_else(|| s.fac_language().map(str::to_string))
                    .unwrap_or_default(),
            })
            .collect()
    }
}

fn build_audio(short_id: u8, params: &AudioParams, stream: StreamLengths) -> Result<AudioPipeline, String> {
    let coding = match params.codec {
        AudioCodec::Aac => DrmAudioCoding::Aac,
        AudioCodec::XheAac => DrmAudioCoding::XheAac,
        AudioCodec::Opus => DrmAudioCoding::Opus,
        AudioCodec::Reserved => return Err("reserved audio coding (CELP/HVXC are not supported)".into()),
    };
    let deframer = AudioDeframer::new(params, stream).map_err(|e| e.to_string())?;
    let decoder = open_decoder(coding, &params.type9_bytes).map_err(|e| e.to_string())?;
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
