//! The running station: produces the transmitter signal frame by frame and writes it
//! to the configured sinks.
//!
//! Per 400 ms transmission frame ([`Station::transmit_frame`]):
//!
//! 1. every audio service encodes 400 ms of input into an audio super frame of exactly
//!    its stream length, with the next text message piece in the last four bytes;
//! 2. every data stream produces its packets ([`decdrm_data::DataEncoder`]);
//! 3. [`multiplex`] assembles the MSC multiplex frame;
//! 4. at the start of each super frame the SDC scheduler builds the SDC data field;
//! 5. the FAC carries the next service of the repetition pattern;
//! 6. the [`Transmitter`] produces 19 200 baseband samples, the [`OutputStage`] turns
//!    them into real IF or I/Q samples, and the file and sound card get them.

use crate::audio::{AudioChain, AudioCounters};
use crate::config::{AppKind, StationConfig};
use crate::error::{Result, StationError};
use crate::fac::FacScheduler;
use crate::output::DeviceSink;
use crate::plan::{FRAME_SECONDS, MultiplexPlan, StreamContent};
use crate::sdc::{SdcScheduler, entities};
use decdrm_core::Cplx;
use decdrm_core::fac::ServiceParams;
use decdrm_core::mux::msc::multiplex;
use decdrm_core::tx::Transmitter;
use decdrm_core::tx::output::OutputStage;
use decdrm_data::DataEncoder;
use decdrm_io::FileWriter;

/// Status of a running station, for user interfaces (see [`Station::status`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StationStatus {
    /// Transmission frames (400 ms each) produced.
    pub frames: u64,
    /// Seconds of signal produced.
    pub seconds: f64,
    /// RMS level of the last frame's output (per channel), dBFS.
    pub output_rms_dbfs: f32,
    /// Peak level of the last frame's output, dBFS.
    pub output_peak_dbfs: f32,
    /// Output samples clipped to full scale so far.
    pub clipped_samples: u64,
    /// Name of the sound card being fed, if any.
    pub device: Option<String>,
    /// Sound-card underruns so far.
    pub device_underruns: u64,
    /// SDC blocks produced.
    pub sdc_blocks: u64,
    /// Data field bytes used by the last SDC block, and the capacity.
    pub sdc_bytes_used: usize,
    pub sdc_capacity: usize,
    /// The last time and date sent in the SDC.
    pub time_sent: Option<String>,
    /// Per service, in Short Id order.
    pub services: Vec<ServiceStatus>,
}

/// Status of one service.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceStatus {
    pub short_id: u8,
    pub label: String,
    pub service_id: u32,
    /// Bit rate of the service's streams (shared data streams split by the requested
    /// bit rates), bit/s.
    pub bitrate: f64,
    pub audio: Option<AudioStatus>,
    pub apps: Vec<AppStatus>,
}

/// Status of an audio service.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioStatus {
    /// E.g. `HE-AAC mono, 12 kHz core`.
    pub codec: String,
    pub stream_id: u8,
    /// Gross bit rate of the audio stream (including super frame header, CRCs and text
    /// message bytes), bit/s.
    pub stream_bitrate: f64,
    /// Bit rate the encoder runs at, bit/s.
    pub encoder_bitrate: u32,
    /// Description of the input.
    pub input: String,
    /// A non-looping input file has ended (silence is being sent).
    pub input_finished: bool,
    /// Levels and counters.
    pub counters: AudioCounters,
}

/// Status of a data application.
#[derive(Debug, Clone, PartialEq)]
pub struct AppStatus {
    pub kind: AppKind,
    pub stream_id: u8,
    pub packet_id: u8,
    /// Share of the stream's bit rate, bit/s.
    pub bitrate: f64,
}

struct DataStream {
    stream: usize,
    len: usize,
    encoder: DataEncoder,
}

/// A DRM30 transmitter station (see the module docs).
///
/// ```no_run
/// # fn main() -> Result<(), decdrm_station::StationError> {
/// let cfg = decdrm_station::StationConfig::load("station.toml")?;
/// let mut station = decdrm_station::Station::new(cfg)?;
/// for _ in 0..25 {
///     station.transmit_frame()?; // 400 ms of signal to the configured outputs
/// }
/// let status = station.finish()?;
/// println!("{} frames, {} clipped samples", status.frames, status.clipped_samples);
/// # Ok(()) }
/// ```
pub struct Station {
    cfg: StationConfig,
    plan: MultiplexPlan,
    tx: Transmitter,
    output: OutputStage,
    file: Option<FileWriter>,
    device: Option<DeviceSink>,
    /// (Short Id, chain) of every audio service.
    audio: Vec<(usize, AudioChain)>,
    data: Vec<DataStream>,
    sdc: SdcScheduler,
    fac: FacScheduler,
    /// UTC time of the first frame, seconds since the Unix epoch.
    start_unix: f64,
    names: Vec<String>,
    status: StationStatus,
    baseband: Vec<Cplx>,
    samples: Vec<f32>,
}

// Compile-time check that a station can be moved to a worker thread (e.g. by a GUI).
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Station>();
};

impl Station {
    /// Validate `cfg`, open the inputs, create the encoders and data carousels, and
    /// open the outputs.
    pub fn new(cfg: StationConfig) -> Result<Self> {
        let plan = cfg.validate()?;
        let tx = Transmitter::new(plan.tx)?;
        let output = OutputStage::new(plan.layout, plan.output)?;
        let names: Vec<String> =
            cfg.services.iter().enumerate().map(|(i, s)| format!("service {i} (\"{}\")", s.label)).collect();

        let mut audio = Vec::new();
        for (i, sp) in plan.services.iter().enumerate() {
            if let Some(a) = &sp.audio {
                let lengths = plan.streams[usize::from(a.stream)].lengths;
                audio.push((i, AudioChain::new(&cfg, i, a, lengths)?));
            }
        }
        let mut data = Vec::new();
        for st in &plan.streams {
            let StreamContent::Data { packet_len, apps } = &st.content else { continue };
            let mut list = Vec::new();
            for r in apps {
                let service = &cfg.services[r.service];
                let app = service.applications().nth(r.index).expect("valid application index");
                let source = crate::data::build_source(&cfg, app, service.id).map_err(|message| StationError::Data {
                    what: format!("{} of {}", app.kind, names[r.service]),
                    message,
                })?;
                list.push((app, r.packet_id, source));
            }
            let encoder = crate::data::stream_encoder(*packet_len, list)
                .map_err(|message| StationError::Data { what: format!("stream {}", st.id), message })?;
            data.push(DataStream { stream: usize::from(st.id), len: st.bytes(), encoder });
        }

        let sdc = SdcScheduler::new(entities(&cfg, &plan), plan.sdc_capacity, cfg.time.enabled, cfg.time.local_offset_minutes)?;
        let fac = FacScheduler::new(
            cfg.services
                .iter()
                .enumerate()
                .map(|(i, s)| ServiceParams {
                    service_id: s.id & 0xFF_FFFF,
                    short_id: i as u8,
                    audio_ca: false,
                    language: s.language.0,
                    is_data: !s.is_audio(),
                    descriptor: if s.is_audio() { s.programme_type.0 } else { s.fac_app_id },
                    data_ca: false,
                })
                .collect(),
        );
        let start_unix = cfg
            .time
            .start
            .as_deref()
            .and_then(crate::time::parse_iso8601)
            .map_or_else(crate::time::system_now, |t| t as f64);

        // The outputs last, so that a configuration error leaves no empty file behind.
        let channels = output.channels();
        let file = crate::output::open_file(&cfg, channels)?;
        let device = DeviceSink::open(&cfg.output, channels)?;

        let mut station = Self {
            status: StationStatus { sdc_capacity: plan.sdc_capacity, ..Default::default() },
            cfg,
            plan,
            tx,
            output,
            file,
            device,
            audio,
            data,
            sdc,
            fac,
            start_unix,
            names,
            baseband: Vec::new(),
            samples: Vec::new(),
        };
        station.update_status();
        Ok(station)
    }

    /// The configuration.
    pub fn config(&self) -> &StationConfig {
        &self.cfg
    }

    /// The multiplex (streams, lengths, codec parameters).
    pub fn plan(&self) -> &MultiplexPlan {
        &self.plan
    }

    /// The current status.
    pub fn status(&self) -> &StationStatus {
        &self.status
    }

    /// Number of output channels (1 real, 2 I/Q) at 48 kHz.
    pub fn output_channels(&self) -> usize {
        self.output.channels()
    }

    /// Whether every audio input is a non-looping file (so the programme ends by
    /// itself); false for data-only stations.
    pub fn inputs_finite(&self) -> bool {
        !self.audio.is_empty() && self.audio.iter().all(|(_, a)| a.input_finite())
    }

    /// Whether every audio input is a non-looping file that has ended.
    pub fn inputs_finished(&self) -> bool {
        self.inputs_finite() && self.audio.iter().all(|(_, a)| a.input_finished())
    }

    /// Produce the next 400 ms transmission frame, write it to the outputs and return
    /// the output samples (interleaved, 48 kHz; see [`Self::output_channels`]).
    pub fn transmit_frame(&mut self) -> Result<&[f32]> {
        let mut frames: Vec<Vec<u8>> = vec![Vec::new(); self.plan.streams.len()];
        for (service, chain) in &mut self.audio {
            frames[usize::from(chain.plan.stream)] = chain.next_logical_frame(&self.names[*service])?;
        }
        for d in &mut self.data {
            frames[d.stream] = d.encoder.next_frame(d.len);
        }
        let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
        let msc = multiplex(&refs, &self.plan.multiplex, self.plan.geometry())?;

        let sdc = if self.tx.frame_index() == 0 {
            let now = self.start_unix + self.status.frames as f64 * FRAME_SECONDS;
            Some(self.sdc.next_block(now.floor() as i64)?)
        } else {
            None
        };
        let fac = self.fac.next_fac();
        self.baseband.clear();
        self.tx.transmit_frame_into(&fac, &msc, sdc.as_deref(), &mut self.baseband)?;
        self.samples.clear();
        self.output.process(&self.baseband, &mut self.samples);
        self.write_outputs()?;
        self.status.frames += 1;
        self.update_levels();
        self.update_status();
        Ok(&self.samples)
    }

    /// Produce `n` frames.
    pub fn run_frames(&mut self, n: u64) -> Result<()> {
        for _ in 0..n {
            self.transmit_frame()?;
        }
        Ok(())
    }

    /// End the transmission: flush the channel filter's tail to the outputs, complete
    /// the file and play out the sound card's buffer. Returns the final status.
    pub fn finish(mut self) -> Result<StationStatus> {
        self.samples.clear();
        self.output.flush(&mut self.samples);
        self.write_outputs()?;
        if let Some(f) = self.file.take() {
            f.finalize().map_err(StationError::Output)?;
        }
        if let Some(d) = self.device.as_mut() {
            d.drain();
        }
        self.update_status();
        Ok(self.status)
    }

    fn write_outputs(&mut self) -> Result<()> {
        if let Some(f) = self.file.as_mut() {
            f.write(&self.samples).map_err(StationError::Output)?;
        }
        if let Some(d) = self.device.as_mut() {
            d.write(&self.samples)?;
        }
        Ok(())
    }

    /// Output levels of the frame just produced.
    fn update_levels(&mut self) {
        let n = self.samples.len().max(1);
        let (sum, peak) = self.samples.iter().fold((0.0f64, 0.0f32), |(s, p), &v| (s + f64::from(v * v), p.max(v.abs())));
        self.status.output_rms_dbfs = 20.0 * ((sum / n as f64).sqrt() as f32).max(1e-10).log10();
        self.status.output_peak_dbfs = 20.0 * peak.max(1e-10).log10();
    }

    fn update_status(&mut self) {
        let st = &mut self.status;
        st.seconds = st.frames as f64 * FRAME_SECONDS;
        st.clipped_samples = self.output.clipped_samples();
        st.device = self.device.as_ref().map(|d| d.name().to_string());
        st.device_underruns = self.device.as_ref().map_or(0, DeviceSink::underruns);
        st.sdc_blocks = self.sdc.blocks;
        st.sdc_bytes_used = self.sdc.last_used;
        st.time_sent = self.sdc.last_time.as_ref().map(crate::time::format_entity);
        st.services = self
            .plan
            .services
            .iter()
            .zip(&self.cfg.services)
            .enumerate()
            .map(|(i, (sp, s))| ServiceStatus {
                short_id: sp.short_id,
                label: s.label.clone(),
                service_id: s.id,
                bitrate: self.plan.service_bitrate(i),
                audio: sp.audio.as_ref().map(|a| {
                    let chain = self.audio.iter().find(|(k, _)| *k == i).map(|(_, c)| c);
                    AudioStatus {
                        codec: a.describe(),
                        stream_id: a.stream,
                        stream_bitrate: self.plan.streams[usize::from(a.stream)].bitrate(),
                        encoder_bitrate: a.encoder_bitrate,
                        input: chain.map(AudioChain::input_description).unwrap_or_default(),
                        input_finished: chain.is_some_and(AudioChain::input_finished),
                        counters: chain.map(|c| c.counters).unwrap_or_default(),
                    }
                }),
                apps: sp
                    .apps
                    .iter()
                    .map(|a| AppStatus { kind: a.kind, stream_id: a.stream, packet_id: a.packet_id, bitrate: a.bitrate })
                    .collect(),
            })
            .collect();
    }
}
