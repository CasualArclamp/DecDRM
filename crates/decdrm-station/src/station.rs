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
use crate::webstream::WebStreamStatus;
use decdrm_core::Cplx;
use decdrm_core::channel::ChannelSimulator;
use decdrm_core::channel::resample::Resampler;
use decdrm_core::fac::ServiceParams;
use decdrm_core::mux::msc::multiplex;
use decdrm_core::tx::Transmitter;
use decdrm_core::tx::output::OutputStage;
use decdrm_data::DataEncoder;
use decdrm_io::FileWriter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// The output file (resolved path), if any.
    pub output_file: Option<PathBuf>,
    /// Name of the sound card being fed, if any.
    pub device: Option<String>,
    /// Sound-card underruns so far.
    pub device_underruns: u64,
    /// Signal queued on the sound card (not yet played), milliseconds.
    pub device_buffer_ms: Option<f64>,
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
    /// A web stream input's state, stream, title and buffer.
    pub web_stream: Option<WebStreamStatus>,
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

/// Stops a station's sound-card waits from another thread (see [`Station::stop_handle`]).
///
/// After [`StopHandle::stop`], writing to the sound card returns at once (the rest of
/// the frame is not queued), [`Station::finish`] does not wait for the queued signal
/// to play out, and a web stream input stops waiting for audio (silence follows); file
/// output is unaffected. The station's owner still ends its frame loop and calls
/// `finish` as usual.
///
/// Rust note: the handle is an `Arc<AtomicBool>` — a flag shared between threads
/// without a lock; `Clone` gives another handle to the same flag.
#[derive(Debug, Clone, Default)]
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    /// Ask the station to stop waiting for the sound card.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether [`Self::stop`] was called.
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
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
    /// Channel simulator (`[simulate]`: fading, frequency offset, noise) and its output.
    simulator: Option<ChannelSimulator>,
    impaired: Vec<Cplx>,
    /// The simulated receiver clock error, applied to the output samples so that it
    /// scales the whole spectrum (the IF too), as a sound card's clock does.
    clock: Option<Resampler>,
    clock_buf: (Vec<Cplx>, Vec<Cplx>),
    samples: Vec<f32>,
    stop: StopHandle,
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
        Self::with_plan(cfg, plan)
    }

    /// Like [`Self::new`] with the plan of an earlier `cfg.validate()` (e.g. one a user
    /// interface has already shown), so the configuration is not checked twice. `plan`
    /// must come from this `cfg`.
    pub fn with_plan(cfg: StationConfig, plan: MultiplexPlan) -> Result<Self> {
        Self::with_plan_and_stop(cfg, plan, StopHandle::default())
    }

    /// Like [`Self::with_plan`] with a stop handle made beforehand, so that another
    /// thread can also end the waits of creating the station: a web stream input waits
    /// for the stream's first audio (up to 20 s), and [`StopHandle::stop`] makes that
    /// fail at once ("stopped while connecting"). [`Self::stop_handle`] returns `stop`.
    pub fn with_plan_and_stop(cfg: StationConfig, plan: MultiplexPlan, stop: StopHandle) -> Result<Self> {
        let tx = Transmitter::new(plan.tx)?;
        let output = OutputStage::new(plan.layout, plan.output)?;
        let names: Vec<String> =
            cfg.services.iter().enumerate().map(|(i, s)| format!("service {i} (\"{}\")", s.label)).collect();

        // Before the inputs: it also ends their waits (a web stream connecting).
        let mut audio = Vec::new();
        for (i, sp) in plan.services.iter().enumerate() {
            if let Some(a) = &sp.audio {
                let lengths = plan.streams[usize::from(a.stream)].lengths;
                audio.push((i, AudioChain::new(&cfg, i, a, lengths, &stop)?));
            }
        }
        let mut data = Vec::new();
        for st in &plan.streams {
            let StreamContent::Data { packet_len, apps } = &st.content else { continue };
            let mut list = Vec::new();
            for r in apps {
                let service = &cfg.services[r.service];
                let app = service.applications().nth(r.index).expect("valid application index");
                let scope = if app.kind == AppKind::Epg { cfg.epg_scope(r.service) } else { service.id };
                let source = crate::data::build_source(&cfg, app, scope).map_err(|message| StationError::Data {
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

        let output_file = cfg.output.file.as_ref().map(|f| cfg.resolve(f));
        let simulator = cfg.simulate.as_ref().and_then(|s| s.channel_config()).map(|c| {
            ChannelSimulator::new(plan.layout, decdrm_core::channel::ChannelConfig { sample_rate_offset_ppm: 0.0, ..c })
        });
        let clock = cfg
            .simulate
            .as_ref()
            .filter(|s| s.sample_rate_offset_ppm != 0.0)
            .map(|s| Resampler::new(1.0 + s.sample_rate_offset_ppm * 1e-6));
        let mut station = Self {
            status: StationStatus { sdc_capacity: plan.sdc_capacity, output_file, ..Default::default() },
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
            simulator,
            impaired: Vec::new(),
            clock,
            clock_buf: (Vec::new(), Vec::new()),
            samples: Vec::new(),
            stop,
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

    /// A handle that stops the station's sound-card waits from another thread (e.g. a
    /// GUI's Stop button while [`Self::transmit_frame`] waits for room on a stalled
    /// sound card).
    pub fn stop_handle(&self) -> StopHandle {
        self.stop.clone()
    }

    /// Log lines of the audio inputs since the last call — a web stream's connections,
    /// redirects, playlists, titles, reconnections, buffer underruns — each prefixed
    /// with its service, e.g. `service 0 ("Radio"): web stream: title: Artist - Song`.
    /// Call it in the frame loop and show the lines.
    pub fn take_log(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        for (service, chain) in &mut self.audio {
            lines.extend(chain.take_log().into_iter().map(|l| format!("{}: {l}", self.names[*service])));
        }
        lines
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
        // Rust note: `signal` borrows either buffer; the borrow checker accepts the
        // mutable use of the other fields below because they are distinct fields.
        let signal = match self.simulator.as_mut() {
            Some(sim) => {
                self.impaired.clear();
                sim.process(&self.baseband, &mut self.impaired);
                &self.impaired
            }
            None => &self.baseband,
        };
        self.samples.clear();
        self.output.process(signal, &mut self.samples);
        self.apply_clock();
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
    /// the file and play out the sound card's buffer (not after [`StopHandle::stop`]).
    /// Returns the final status.
    pub fn finish(mut self) -> Result<StationStatus> {
        self.samples.clear();
        self.output.flush(&mut self.samples);
        self.apply_clock();
        self.write_outputs()?;
        if let Some(f) = self.file.take() {
            f.finalize().map_err(StationError::Output)?;
        }
        if let Some(d) = self.device.as_mut() {
            d.drain(&self.stop);
        }
        self.update_status();
        Ok(self.status)
    }

    /// Resample `samples` for the simulated receiver clock error, if any.
    fn apply_clock(&mut self) {
        let Some(r) = self.clock.as_mut() else { return };
        let ch = self.output.channels();
        let (input, output) = &mut self.clock_buf;
        input.clear();
        input.extend(
            self.samples
                .chunks_exact(ch)
                .map(|f| Cplx::new(f64::from(f[0]), f.get(1).map_or(0.0, |&q| f64::from(q)))),
        );
        output.clear();
        r.process(input, output);
        self.samples.clear();
        for c in output.iter() {
            self.samples.push(c.re as f32);
            if ch > 1 {
                self.samples.push(c.im as f32);
            }
        }
    }

    fn write_outputs(&mut self) -> Result<()> {
        if let Some(f) = self.file.as_mut() {
            f.write(&self.samples).map_err(StationError::Output)?;
        }
        if let Some(d) = self.device.as_mut() {
            d.write(&self.samples, &self.stop)?;
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
        st.device_buffer_ms = self.device.as_ref().map(|d| d.queued().as_secs_f64() * 1000.0);
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
                        web_stream: chain.and_then(AudioChain::web_stream),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AudioInputSettings, AudioSettings, Codec, OutputSettings, ServiceSettings};

    #[test]
    fn with_plan_status_and_stop() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = ServiceSettings::new("Test", 0x42);
        service.audio = Some(AudioSettings::new(Codec::HeAac, AudioInputSettings::tone(440.0)));
        let cfg = StationConfig {
            output: OutputSettings { file: Some("out.wav".into()), ..Default::default() },
            services: vec![service],
            base_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let plan = cfg.validate().unwrap();
        let mut station = Station::with_plan(cfg, plan).unwrap();
        let wav = dir.path().join("out.wav");
        assert_eq!(station.status().output_file.as_deref(), Some(wav.as_path()));
        assert_eq!(station.status().device_buffer_ms, None, "no sound card");
        station.transmit_frame().unwrap();
        // A stop request only concerns sound-card waits: the file still gets everything.
        let stop = station.stop_handle();
        assert!(!stop.is_stopped());
        stop.stop();
        assert!(station.stop_handle().is_stopped(), "handles share one flag");
        station.transmit_frame().unwrap();
        let status = station.finish().unwrap();
        assert_eq!(status.frames, 2);
        let bytes = std::fs::metadata(&wav).unwrap().len();
        assert!(bytes > 2 * 19_200 * 2, "{bytes} bytes");
    }
}
