//! Validation of a [`StationConfig`] and the resulting [`MultiplexPlan`]: which stream
//! carries what, how long every stream is, and the parameters of every encoder.
//!
//! # Stream allocation
//!
//! Every audio service gets a stream, and every data application a stream of its own
//! unless several name the same `stream` (packet mode multiplexes up to four packet ids
//! into one stream). With hierarchical modulation the marked stream becomes stream 0
//! and fills the very strongly protected part (its length is fixed by the channel);
//! the others follow in configuration order (ES 201 980 §6.2.3).
//!
//! Data streams get what they ask for, rounded up to whole packets per frame. The
//! audio streams share the rest of the multiplex frame in proportion to their `share`.
//! With unequal error protection (streams in part A) the MSC capacity itself depends
//! on the part A length (the MLC's N₁, §7.2.1.1), so the largest total audio length
//! that fits is found by bisection over the real MLC parameters; the need grows faster
//! than any capacity gained, so feasibility is monotonic.
//!
//! # Audio bit rates
//!
//! An AAC audio super frame of `L` bytes carries a header of frame borders, one CRC
//! byte per frame and the frames (§5.4.1); a 4-byte text message piece follows when
//! the service has text. FDK runs in constant-bit-rate mode, but the DRM re-packing
//! changes each frame's size (see `decdrm_codecs::FdkEncoderConfig`) and the 5 or 10
//! frames must fit the payload together. The encoder bit rate is therefore chosen from
//! a table of measured worst-case super frame sizes per profile (`worst_fill`, measured
//! by the ignored test `tests/fdk_fill.rs`): the
//! highest rate, at most 97 % of the payload, whose worst case fills at most 98.5 % —
//! typically 94–97 %, less for stereo AAC with a 24 kHz core, which overshoots its
//! budget by up to 10 % at low rates. A super frame that still overflows drops a frame,
//! which the receiver conceals. Opus packets are constant size: ⌊payload / 20⌋ bytes.
//! DAC (DecDRM's neural codec extension) takes the highest of its fixed bit rates that
//! fits and spends the rest on CRC granularity and repetition (`decdrm_dac::plan`).
//!
//! # xHE-AAC
//!
//! xHE-AAC (§5.3.1) has no fixed number of frames per super frame: access units of
//! varying size run continuously through the super frames, and the encoder's rate
//! control (`decdrm_codecs::XheAacEncoder`) fills the stream exactly — the super frame
//! (stream minus text bytes) less a 2-byte header and 4 bytes of CRC and directory per
//! frame; the frame sizes vary within the bit reservoir. The sampling rate (the rate the
//! encoder takes and the decoder delivers, signalled in SDC type 9) is `sample_rate` if
//! set, else it follows from the super frame's bit rate ([`default_xhe_rate`]):
//!
//! | super frame bit rate | sampling rate | with the default 2:1 SBR |
//! |----------------------|---------------|--------------------------|
//! | up to 24 kbit/s      | 24 kHz        | 12 kHz core, audio to 12 kHz |
//! | up to 48 kbit/s      | 32 kHz        | 16 kHz core, audio to 16 kHz |
//! | above                | 48 kHz        | 24 kHz core, audio to 24 kHz |
//!
//! A low core rate leaves the core coder the most bits per spectral line, which is what
//! matters at low rates (DecDRM's xHE-AAC sweep: 54–59 dB tone SNR at 24 kHz from
//! 8 kbit/s; 32 kHz stereo is poor at 8 kbit/s, 48 kHz stereo below ~24 kbit/s). The
//! SBR ratio is `sbr_ratio`, by default the encoder's choice for the rate
//! (`XheAacConfig::sbr_ratio`); `sbr_ratio = "4:1"` defaults to 48 kHz, `"none"` to at
//! most 32 kHz. The plan checks the configuration with `XheAacConfig::budget` (too
//! small a stream, unsupported rate/ratio/channel combinations such as 38.4 kHz stereo
//! or 4:1 stereo) and then creates the encoder once, for the xHE-AAC Static Config that
//! SDC type 9 carries.

use crate::config::{AppKind, Codec, Part, SbrRatio, SignalFormat, StationConfig};
use crate::error::{ConfigProblems, Result, StationError};
use crate::sdc;
use decdrm_core::cellmap::CellMap;
use decdrm_core::fac::MscMode;
use decdrm_core::fec::mlc::{MlcParams, MscProtection};
use decdrm_core::mux::audio::{AacSuperFrameFormat, OPUS_FRAMES_PER_SUPER_FRAME, TEXT_MESSAGE_BYTES};
use decdrm_core::mux::msc::MscGeometry;
use decdrm_core::mux::sdc::{MultiplexDescription, StreamLengths};
use decdrm_core::mux::service::{AudioCodec, AudioMode, AudioParams};
use decdrm_codecs::{XHE_AAC_SAMPLE_RATES, XheAacConfig, XheAacEncoder};
use decdrm_core::params::{ChannelLayout, MAX_SERVICES, MAX_STREAMS};
use decdrm_core::tx::output::{OutputConfig, OutputFormat, OutputStage, suggested_if_hz};
use decdrm_core::tx::{MscCapacity, Transmitter, TxConfig};
use decdrm_io::Container;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Seconds per multiplex frame.
pub const FRAME_SECONDS: f64 = 0.4;

/// Highest fraction of the AAC payload the encoder's bit rate is set to (see the
/// module docs).
const AAC_FILL: f64 = 0.97;
/// Largest fraction of the AAC payload the worst measured super frame may fill.
const AAC_SAFETY: f64 = 0.985;

/// Opus needs at least this many bytes per 20 ms packet (6 kbit/s).
const OPUS_MIN_PACKET: usize = 15;
/// Largest Opus packet.
const OPUS_MAX_PACKET: usize = decdrm_codecs::OPUS_MAX_PACKET;

/// Largest stream length the multiplex description can express (12-bit fields).
const MAX_STREAM_BYTES: usize = 4095;

/// The resolved multiplex: transmitter parameters, streams and services.
#[derive(Debug, Clone)]
pub struct MultiplexPlan {
    /// Transmitter configuration (FAC channel parameters, protection, part A length).
    pub tx: TxConfig,
    pub layout: ChannelLayout,
    /// MSC bits per multiplex frame.
    pub capacity: MscCapacity,
    /// SDC data field bytes per super frame.
    pub sdc_capacity: usize,
    /// SDC type 0 entity content.
    pub multiplex: MultiplexDescription,
    /// Streams, indexed by stream id.
    pub streams: Vec<StreamPlan>,
    /// Services, indexed by Short Id.
    pub services: Vec<ServicePlan>,
    /// Output stage settings (real IF or I/Q, level, filter).
    pub output: OutputConfig,
    /// A modulator (`[mdi]`): where the MDI comes from. The channel, streams and
    /// services above are placeholders until the MDI arrives.
    pub mdi: Option<String>,
}

/// One MSC stream.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamPlan {
    pub id: u8,
    /// Bytes per multiplex frame in part A / part B (the hierarchical stream: part B).
    pub lengths: StreamLengths,
    /// Carried in the very strongly protected part (HMsym/HMmix).
    pub hierarchical: bool,
    pub content: StreamContent,
}

impl StreamPlan {
    /// Bytes per multiplex frame.
    pub fn bytes(&self) -> usize {
        self.lengths.total()
    }

    /// Gross bit rate of the stream, bit/s.
    pub fn bitrate(&self) -> f64 {
        self.bytes() as f64 * 8.0 / FRAME_SECONDS
    }
}

/// What a stream carries.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamContent {
    /// The audio of a service (Short Id).
    Audio { service: usize },
    /// Packet-mode data applications with packets of `packet_len` bytes (incl. the
    /// 3 bytes of header and CRC).
    Data { packet_len: usize, apps: Vec<AppRef> },
}

/// A data application: service (Short Id), index into
/// [`crate::ServiceSettings::applications`], and its packet id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppRef {
    pub service: usize,
    pub index: usize,
    pub packet_id: u8,
}

/// One service.
#[derive(Debug, Clone)]
pub struct ServicePlan {
    pub short_id: u8,
    pub audio: Option<AudioPlan>,
    pub apps: Vec<AppPlan>,
}

/// The audio coding of a service.
#[derive(Debug, Clone)]
pub struct AudioPlan {
    pub stream: u8,
    pub codec: Codec,
    /// Core coder sampling rate: AAC's core rate, 48 000 for Opus, xHE-AAC's core rate
    /// (the sampling rate divided by the SBR ratio).
    pub core_rate: u32,
    /// Stereo coding (for HE-AAC v2: parametric stereo).
    pub stereo: bool,
    /// Sampling rate of the PCM the encoder takes (2 × core with SBR, 48 kHz Opus,
    /// xHE-AAC's sampling rate).
    pub input_rate: u32,
    /// Channels of the PCM the encoder takes.
    pub input_channels: usize,
    /// Text messages in the last four bytes of the stream.
    pub text: bool,
    /// Audio frames per 400 ms super frame (5, 10 or 20; xHE-AAC: the average rounded
    /// up).
    pub frames_per_super_frame: usize,
    /// Audio super frame bytes (stream minus text bytes).
    pub super_frame_len: usize,
    /// Bytes available for the coded frames (super frame minus header and CRC bytes;
    /// xHE-AAC: also minus the directory, on average).
    pub payload_len: usize,
    /// Encoder bit rate, bit/s (Opus: packet size × 400 bit/s; xHE-AAC: the channel
    /// capacity for access units, which the encoder fills).
    pub encoder_bitrate: u32,
    /// Opus packet size in bytes (0 for AAC).
    pub opus_packet_bytes: usize,
    /// SDC type 9 parameters.
    pub params: AudioParams,
    /// DAC: bandwidth and DRM framing (signalled in `params`).
    pub dac: Option<decdrm_dac::DacConfig>,
    /// xHE-AAC: the encoder configuration (its Static Config is in `params`).
    pub xhe: Option<XheAacConfig>,
}

/// `hz` in kHz, with a decimal where needed ("24", "38.4").
fn khz(hz: u32) -> String {
    if hz.is_multiple_of(1000) { format!("{}", hz / 1000) } else { format!("{:.1}", f64::from(hz) / 1000.0) }
}

impl AudioPlan {
    /// E.g. `HE-AAC mono, 12 kHz core`.
    pub fn describe(&self) -> String {
        let ch = match (self.codec, self.stereo) {
            (Codec::HeAacV2, _) => "parametric stereo",
            (_, true) => "stereo",
            _ => "mono",
        };
        match self.codec {
            Codec::Opus => format!("Opus {ch}, 48 kHz"),
            Codec::Aac => format!("AAC {ch}, {} kHz", self.core_rate / 1000),
            Codec::HeAac => format!("HE-AAC {ch}, {} kHz core", self.core_rate / 1000),
            Codec::HeAacV2 => format!("HE-AAC v2 ({ch}), {} kHz core", self.core_rate / 1000),
            Codec::XheAac if self.core_rate == self.input_rate => {
                format!("xHE-AAC {ch}, {} kHz (no SBR)", khz(self.input_rate))
            }
            Codec::XheAac => format!("xHE-AAC {ch}, {} kHz ({} kHz core)", khz(self.input_rate), khz(self.core_rate)),
            Codec::Dac => match self.dac {
                Some(c) => format!("{}, 24 kHz mono", c.describe()),
                None => "DAC, 24 kHz mono".into(),
            },
        }
    }
}

/// A data application of a service.
#[derive(Debug, Clone, PartialEq)]
pub struct AppPlan {
    pub kind: AppKind,
    /// The user application it is signalled as (SDC type 5).
    pub user_app: decdrm_data::UserApplication,
    pub stream: u8,
    pub packet_id: u8,
    /// Packet data field bytes (SDC type 5 "packet length").
    pub packet_length: u8,
    /// Share of the stream's bit rate attributed to this application, bit/s.
    pub bitrate: f64,
}

impl MultiplexPlan {
    /// The MSC geometry for [`decdrm_core::mux::msc::multiplex`].
    pub fn geometry(&self) -> MscGeometry {
        MscGeometry { vspp_bits: self.capacity.vspp_bits, hpp_bits: self.capacity.hpp_bits, lpp_bits: self.capacity.lpp_bits }
    }

    /// Total bit rate of the streams of a service (shared streams are split by the
    /// applications' requested bit rates), bit/s.
    pub fn service_bitrate(&self, short_id: usize) -> f64 {
        let Some(s) = self.services.get(short_id) else { return 0.0 };
        let audio = s.audio.as_ref().map_or(0.0, |a| self.streams[usize::from(a.stream)].bitrate());
        audio + s.apps.iter().map(|a| a.bitrate).sum::<f64>()
    }

    /// Human-readable summary (for the CLI).
    pub fn describe(&self, cfg: &StationConfig) -> String {
        if let Some(input) = &self.mdi {
            return format!(
                "modulator: MDI from {input}; the channel and the services come from the MDI{}\n",
                if cfg.services.is_empty() { "" } else { " (the file's services are not used)" }
            );
        }
        let t = &self.tx;
        let mut s = String::new();
        let prot = if self.streams.iter().any(|st| st.lengths.part_a > 0) {
            format!("protection A {} / B {}", t.protection.part_a, t.protection.part_b)
        } else {
            format!("protection {}", t.protection.part_b)
        };
        let hier = if t.msc_mode.is_hierarchical() { format!(", hierarchical {}", t.protection.hierarchical) } else { String::new() };
        let _ = writeln!(
            s,
            "mode {} / {} kHz (SO {}), MSC {} {prot}{hier}, SDC {}, {} interleaving",
            t.mode,
            t.occupancy.bandwidth_khz(),
            t.occupancy.value(),
            crate::config::MscModeSetting(t.msc_mode),
            crate::config::SdcModeSetting(t.sdc_mode),
            crate::config::InterleavingSetting(t.interleaving),
        );
        let total = self.capacity.total_bits();
        let _ = writeln!(
            s,
            "MSC {} bytes per frame ({:.2} kbit/s), SDC {} bytes per super frame",
            total / 8,
            total as f64 / FRAME_SECONDS / 1000.0,
            self.sdc_capacity
        );
        for st in &self.streams {
            let part = if st.hierarchical {
                "hierarchical"
            } else if st.lengths.part_a > 0 {
                "part A"
            } else {
                "part B"
            };
            let what = match &st.content {
                StreamContent::Audio { service } => {
                    let a = self.services[*service].audio.as_ref().expect("audio stream has audio");
                    format!(
                        "service {service} \"{}\": {}, encoder {:.2} kbit/s{}",
                        cfg.services[*service].label,
                        a.describe(),
                        f64::from(a.encoder_bitrate) / 1000.0,
                        if a.text { ", text messages" } else { "" }
                    )
                }
                StreamContent::Data { packet_len, apps } => {
                    let list: Vec<String> = apps
                        .iter()
                        .map(|r| {
                            let app = cfg.services[r.service].applications().nth(r.index).expect("valid app index");
                            let kind = match app.kind {
                                AppKind::Raw => format!("raw {:#05X}", app.app_id.unwrap_or(0)),
                                kind => kind.to_string(),
                            };
                            format!("{kind} of service {} (packet id {})", r.service, r.packet_id)
                        })
                        .collect();
                    format!("{} ({}-byte packets)", list.join(", "), packet_len)
                }
            };
            let _ = writeln!(s, "stream {}: {} bytes ({:.2} kbit/s) {part} - {what}", st.id, st.bytes(), st.bitrate() / 1000.0);
        }
        if let Some(sim) = &cfg.simulate {
            let _ = writeln!(s, "channel simulator: {}", sim.describe());
        }
        let afs = &cfg.afs;
        if !afs.is_empty() {
            let count = |n: usize, what: &str| match n {
                0 => None,
                1 => Some(format!("1 {what}")),
                n => Some(format!("{n} {what}s")),
            };
            let parts: Vec<String> = [
                count(afs.multiplexes.len(), "frequency list"),
                count(afs.others.len(), "other-system list"),
                count(afs.schedules.len(), "schedule"),
                count(afs.regions.len(), "region"),
            ]
            .into_iter()
            .flatten()
            .collect();
            let _ = writeln!(s, "alternative frequencies: {}", parts.join(", "));
        }
        s
    }
}

// ---------------------------------------------------------------------------------
// Codec limits
// ---------------------------------------------------------------------------------

/// Default AAC core rate of a codec (xHE-AAC: see [`default_xhe_rate`]).
fn default_core_rate(codec: Codec) -> u32 {
    match codec {
        Codec::Aac | Codec::Dac | Codec::XheAac => 24_000,
        Codec::HeAac | Codec::HeAacV2 => 12_000,
        Codec::Opus => 48_000,
    }
}

/// The xHE-AAC sampling rate for audio super frames of `super_frame_len` bytes when
/// `sample_rate` is not set (see the module docs): 24 kHz up to 24 kbit/s, 32 kHz up to
/// 48 kbit/s, 48 kHz above; 48 kHz for 4:1 SBR (a 12 kHz core) and at most 32 kHz
/// without SBR (more frames than a super frame can list above that).
pub fn default_xhe_rate(super_frame_len: usize, sbr: SbrRatio) -> u32 {
    let bitrate = 20 * super_frame_len;
    let rate = match bitrate {
        0..=24_000 => 24_000,
        24_001..=48_000 => 32_000,
        _ => 48_000,
    };
    match sbr {
        SbrRatio::Ratio4To1 => 48_000,
        SbrRatio::None => rate.min(32_000),
        _ => rate,
    }
}

/// Encoder bit rates (bit/s) at which [`worst_fill`] was measured.
const FILL_POINTS: [f64; 13] =
    [4e3, 6e3, 8e3, 10e3, 12e3, 14e3, 16e3, 18e3, 20e3, 24e3, 28e3, 32e3, 40e3];

/// Largest size of an audio super frame's AAC frames relative to their budget
/// (encoder bit rate × 400 ms), at the encoder bit rates of [`FILL_POINTS`].
///
/// Measured with FDK-AAC 2.0.3 through `decdrm_codecs::FdkDrmEncoder`: the worst run of
/// 5 or 10 consecutive frames over 160 s of two demanding test signals (harmonics with
/// vibrato plus noise bursts; tones in switched white noise). Values above 1 are FDK's
/// constant-bit-rate frames growing in the DRM re-packing (stereo, 24 kHz core), or
/// FDK refusing to go below its own minimum rate (it silently raises such requests).
fn worst_fill(codec: Codec, core_rate: u32, stereo: bool) -> [f64; 13] {
    let stereo_core = stereo && codec != Codec::HeAacV2;
    match (codec, core_rate, stereo_core) {
        (Codec::Aac, 12_000, false) => {
            [1.030, 0.997, 0.940, 0.948, 0.962, 0.970, 0.979, 0.980, 0.980, 0.978, 0.982, 0.989, 0.959]
        }
        (Codec::Aac, 12_000, true) => {
            [1.125, 1.070, 1.080, 1.036, 1.018, 0.993, 0.948, 0.959, 0.949, 0.956, 0.964, 0.971, 0.986]
        }
        (Codec::Aac, _, false) => [1.660, 1.107, 1.042, 1.028, 1.002, 0.981, 0.963, 0.968, 0.932, 0.943, 0.969, 0.984, 0.976],
        (Codec::Aac, _, true) => [2.025, 1.350, 1.095, 1.100, 1.078, 1.051, 1.038, 1.030, 1.026, 1.009, 0.992, 0.972, 0.936],
        (Codec::HeAacV2, 12_000, _) => {
            [1.920, 1.280, 0.960, 0.960, 0.950, 0.950, 0.955, 0.962, 0.977, 0.968, 0.980, 0.985, 0.980]
        }
        (Codec::HeAacV2, _, _) => [2.975, 1.983, 1.488, 1.190, 0.992, 0.984, 0.979, 0.977, 0.960, 0.956, 0.937, 0.968, 0.982],
        (_, 12_000, false) => [1.910, 1.273, 0.948, 0.938, 0.947, 0.959, 0.959, 0.963, 0.966, 0.971, 0.975, 0.979, 0.974],
        (_, 12_000, true) => [3.925, 2.617, 1.962, 1.570, 1.308, 1.121, 0.981, 0.962, 0.931, 0.951, 0.956, 0.958, 0.963],
        (_, _, false) => [2.990, 1.993, 1.495, 1.196, 0.997, 0.983, 0.974, 0.972, 0.952, 0.940, 0.937, 0.956, 0.954],
        (_, _, true) => [4.085, 2.723, 2.042, 1.634, 1.362, 1.167, 1.021, 1.014, 1.004, 1.018, 1.004, 0.989, 0.942],
    }
}

/// Worst-case output bit rate of the encoder running at `bitrate`: [`worst_fill`]
/// interpolated linearly, times the bit rate. Below the first point the output is
/// taken as constant (FDK's floor), above the last one the fill as at most 0.99.
fn worst_output(table: &[f64; 13], bitrate: f64) -> f64 {
    let (first, last) = (FILL_POINTS[0], FILL_POINTS[FILL_POINTS.len() - 1]);
    if bitrate <= first {
        return table[0] * first;
    }
    if bitrate >= last {
        return table[table.len() - 1].max(0.99) * bitrate;
    }
    let k = FILL_POINTS.iter().position(|&p| p > bitrate).expect("inside the table");
    let t = (bitrate - FILL_POINTS[k - 1]) / (FILL_POINTS[k] - FILL_POINTS[k - 1]);
    (table[k - 1] + t * (table[k] - table[k - 1])) * bitrate
}

/// The AAC encoder bit rate for an audio payload of `payload_rate` bit/s: the highest
/// rate (at most 97 % of the payload) whose worst measured super frame fills at most
/// [`AAC_SAFETY`] of the payload. `Err(minimum payload rate)` if none does.
fn aac_encoder_bitrate(codec: Codec, core_rate: u32, stereo: bool, payload_rate: f64) -> std::result::Result<u32, f64> {
    let table = worst_fill(codec, core_rate, stereo);
    let limit = AAC_SAFETY * payload_rate;
    let mut b = (AAC_FILL * payload_rate).min(f64::from(aac_max_bitrate(codec, core_rate, stereo)));
    while b >= 2000.0 {
        if worst_output(&table, b) <= limit {
            return Ok(b as u32);
        }
        b *= 0.99;
    }
    Err(worst_output(&table, FILL_POINTS[0]) / AAC_SAFETY)
}

/// Largest encoder bit rate: 90 % of AAC's limit of 6144 bits per channel and frame
/// (ISO/IEC 14496-3) at the core rate's frame rate (960-sample frames). (The DRM
/// re-packing in `decdrm-codecs` used to fail above ~50 kbit/s per channel because of
/// a too strict HCR segment limit; that is fixed.)
fn aac_max_bitrate(codec: Codec, core_rate: u32, stereo: bool) -> u32 {
    let channels = if stereo && codec != Codec::HeAacV2 { 2.0 } else { 1.0 };
    let frames_per_s = f64::from(core_rate) / 960.0;
    (0.9 * 6144.0 * frames_per_s * channels) as u32
}

// ---------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------

/// Problems found so far, each prefixed with where it was found.
#[derive(Default)]
struct Problems(Vec<String>);

impl Problems {
    fn push(&mut self, s: impl Into<String>) {
        self.0.push(s.into());
    }

    fn finish(self) -> Result<()> {
        if self.0.is_empty() { Ok(()) } else { Err(StationError::Config(ConfigProblems(self.0))) }
    }
}

/// A stream to be allocated.
struct Request {
    content: StreamContent,
    part: Part,
    hierarchical: bool,
    /// Data: requested bytes per frame (whole packets). Audio: 0.
    bytes: usize,
    /// Audio: share weight.
    share: f64,
    /// Data: requested bit rate per application (for status attribution).
    app_bitrates: Vec<f64>,
}

fn service_name(cfg: &StationConfig, i: usize) -> String {
    match cfg.services.get(i) {
        Some(s) if !s.label.is_empty() => format!("service {i} (\"{}\")", s.label),
        _ => format!("service {i}"),
    }
}

impl StationConfig {
    /// Check the whole configuration — channel, output, clock, services, codec and
    /// data application parameters, input files, SDC and MSC capacity — and work out
    /// the multiplex. Every problem found is reported (see
    /// [`StationError::problems`]).
    pub fn validate(&self) -> Result<MultiplexPlan> {
        build(self)
    }
}

/// The output stage settings for `layout` (the IF, unless set, follows the bandwidth).
pub(crate) fn output_config(out: &crate::config::OutputSettings, layout: Option<ChannelLayout>) -> OutputConfig {
    OutputConfig {
        format: match out.format {
            SignalFormat::Real => OutputFormat::Real { if_hz: out.if_hz.unwrap_or_else(|| layout.map_or(12_000.0, suggested_if_hz)) },
            SignalFormat::Iq => OutputFormat::Iq { offset_hz: out.iq_offset_hz, swap: out.iq_swap },
        },
        level_dbfs: out.level_dbfs,
        band_limit: out.band_limit,
    }
}

/// Checks of `[output]`.
fn check_output(cfg: &StationConfig, p: &mut Problems) {
    let out = &cfg.output;
    if out.file.is_none() && out.device.is_none() {
        p.push("output: set `file` and/or `device`");
    }
    if let Some(f) = &out.file {
        let path = cfg.resolve(f);
        match Container::from_path(&path) {
            None => p.push(format!("output: {} must end in .wav or .flac", path.display())),
            Some(Container::Flac) if out.sample_format == crate::config::SampleFormat::Float32 => {
                p.push("output: FLAC files hold int16 or int24 samples, not float32")
            }
            _ => {}
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
            && !dir.is_dir()
        {
            p.push(format!("output: directory {} does not exist", dir.display()));
        }
    }
    if !(-60.0..=0.0).contains(&out.level_dbfs) {
        p.push(format!("output: level_dbfs {} is outside -60..0 dBFS", out.level_dbfs));
    }
}

/// Checks of `[simulate]`.
fn check_simulate(cfg: &StationConfig, p: &mut Problems) {
    if let Some(sim) = &cfg.simulate {
        if !(1..=6).contains(&sim.channel) {
            p.push(format!("simulate: channel {} is not a DRM channel model (1-6)", sim.channel));
        }
        if let Some(snr) = sim.snr_db
            && !(-20.0..=80.0).contains(&snr)
        {
            p.push(format!("simulate: snr_db {snr} is outside -20..80 dB"));
        }
        if !(-2000.0..=2000.0).contains(&sim.frequency_offset_hz) {
            p.push(format!("simulate: frequency_offset_hz {} is outside ±2000 Hz", sim.frequency_offset_hz));
        }
        if !(-5000.0..=5000.0).contains(&sim.sample_rate_offset_ppm) {
            p.push(format!("simulate: sample_rate_offset_ppm {} is outside ±5000 ppm", sim.sample_rate_offset_ppm));
        }
    }
}

/// A modulator's plan: the output, and placeholders for what the MDI brings.
fn build_mdi(cfg: &StationConfig, m: &crate::config::MdiSettings) -> Result<MultiplexPlan> {
    let mut p = Problems::default();
    let origin = match crate::modulator::origin(m, cfg.base_dir.as_deref()) {
        Ok(o) => Some(o),
        Err(e) => {
            p.push(e);
            None
        }
    };
    if !(1..=50).contains(&m.buffer_frames) {
        p.push(format!("mdi: buffer_frames {} is outside 1-50", m.buffer_frames));
    }
    check_output(cfg, &mut p);
    check_simulate(cfg, &mut p);
    p.finish()?;
    let tx = TxConfig::default();
    let layout = ChannelLayout::new(tx.mode, tx.occupancy).expect("the default layout exists");
    let transmitter = Transmitter::new(tx)?;
    let capacity = transmitter.msc_capacity();
    // One stream filling the frame: a valid description for the parts of the station
    // built before the MDI arrives (the modulator does not use them).
    let whole = StreamLengths { part_a: 0, part_b: capacity.main_bits() / 8 };
    Ok(MultiplexPlan {
        tx,
        layout,
        capacity,
        sdc_capacity: transmitter.sdc_capacity_bytes(),
        multiplex: MultiplexDescription::new(0, tx.protection.part_b as u8, &[whole]),
        streams: Vec::new(),
        services: Vec::new(),
        output: output_config(&cfg.output, None),
        mdi: origin.map(|o| o.describe()),
    })
}

fn build(cfg: &StationConfig) -> Result<MultiplexPlan> {
    if let Some(m) = &cfg.mdi {
        return build_mdi(cfg, m);
    }
    let mut p = Problems::default();
    let ch = &cfg.channel;

    // --- Channel.
    let layout = match ch.spectrum_occupancy() {
        None => {
            p.push(format!("channel: occupancy {} is outside 0-5", ch.occupancy));
            None
        }
        Some(so) => {
            let l = ChannelLayout::new(ch.mode.0, so);
            if l.is_none() {
                p.push(format!("channel: robustness mode {} is only defined with occupancy 3 or 5", ch.mode));
            }
            l
        }
    };
    let msc_mode = ch.msc_mode.0;
    let max_level = if msc_mode == MscMode::Qam16Sm { 1 } else { 3 };
    // Part A's level is only used (and checked) when a stream is in part A.
    let uep = uses_part_a(cfg);
    for (what, level, max, used) in [
        ("protection_a", ch.protection_a, max_level, uep),
        ("protection_b", ch.protection_b, max_level, true),
        ("protection_hierarchical", ch.protection_hierarchical, 3, true),
    ] {
        if used && level > max {
            p.push(format!("channel: {what} {level} is out of range 0-{max} for {}", ch.msc_mode));
        }
    }
    // Part A is the higher protected part (§6.4.3.1): a lower level, a lower code rate.
    if uep && ch.protection_a >= ch.protection_b {
        p.push(format!(
            "channel: part A must be protected more strongly than part B, so protection_a ({}) must be below protection_b ({}){}",
            ch.protection_a,
            ch.protection_b,
            if ch.protection_b == 0 { "; raise protection_b or move the part A streams back to part B" } else { "" }
        ));
    }

    // --- Output.
    check_output(cfg, &mut p);
    let output = output_config(&cfg.output, layout);
    if let Some(l) = layout
        && let Err(e) = OutputStage::new(l, output)
    {
        p.push(format!("output: {e} (choose another if_hz / iq_offset_hz)"));
    }

    // --- Clock.
    if let Some(start) = &cfg.time.start
        && crate::time::parse_iso8601(start).is_none()
    {
        p.push(format!("time: start \"{start}\" is not an ISO 8601 time such as 2026-09-29T18:00:00Z"));
    }
    if let Some(m) = cfg.time.local_offset_minutes
        && m.abs() > 31 * 30
    {
        p.push(format!("time: local_offset_minutes {m} is outside ±930"));
    }

    // --- Services.
    if cfg.services.is_empty() {
        p.push("no services: add at least one [[service]]");
    }
    if cfg.services.len() > MAX_SERVICES {
        p.push(format!("{} services configured, DRM allows at most {MAX_SERVICES}", cfg.services.len()));
    }
    let mut ids = BTreeMap::new();
    for (i, s) in cfg.services.iter().enumerate() {
        let name = service_name(cfg, i);
        if s.label.trim().is_empty() {
            p.push(format!("{name}: empty label"));
        }
        if s.label.chars().count() > 16 || s.label.len() > 64 {
            p.push(format!("{name}: label is longer than 16 characters"));
        }
        if s.id > 0xFF_FFFF {
            p.push(format!("{name}: service id {:#X} does not fit in 24 bits", s.id));
        }
        if let Some(j) = ids.insert(s.id, i) {
            p.push(format!("{name}: service id {:#08X} is also used by service {j}", s.id));
        }
        if let Some(l) = &s.iso_language
            && (l.len() != 3 || !l.chars().all(|c| c.is_ascii_alphabetic()))
        {
            p.push(format!("{name}: iso_language \"{l}\" is not a three-letter ISO 639-2 code"));
        }
        if let Some(c) = &s.iso_country
            && (c.len() != 2 || !c.chars().all(|c| c.is_ascii_alphabetic()))
        {
            p.push(format!("{name}: iso_country \"{c}\" is not a two-letter ISO 3166 code"));
        }
        if s.fac_app_id > 31 {
            p.push(format!("{name}: fac_app_id {} is out of range 0-31", s.fac_app_id));
        }
        if s.is_audio() && s.fac_app_id != 0 {
            p.push(format!("{name}: fac_app_id is only used by data services (audio services have a programme_type)"));
        }
        if !s.is_audio() && s.programme_type.0 != 0 {
            p.push(format!("{name}: programme_type is only used by audio services (data services have a fac_app_id)"));
        }
        match (&s.audio, &s.data) {
            (None, None) => p.push(format!("{name}: needs [service.audio] (audio service) or [service.data] (data service)")),
            (Some(_), Some(_)) => p.push(format!(
                "{name}: an audio service lists its data applications as [[service.app]], not [service.data]"
            )),
            _ => {}
        }
        if let Some(a) = &s.audio {
            check_audio(cfg, &name, a, &mut p);
        }
        for (k, app) in s.applications().enumerate() {
            let name = format!("{name}, application {k} ({})", app.kind);
            if !(1..=255).contains(&app.packet_length) {
                p.push(format!("{name}: packet_length {} is outside 1-255", app.packet_length));
            }
            if let Some(id) = app.packet_id
                && id > 3
            {
                p.push(format!("{name}: packet_id {id} is outside 0-3"));
            }
            if app.bitrate == 0 {
                p.push(format!("{name}: bitrate must be positive"));
            }
            match (app.kind, app.app_id) {
                (AppKind::Raw, None) => p.push(format!("{name}: needs `app_id`, the user application type (0x000-0x7FF)")),
                (AppKind::Raw, Some(id)) if id > 0x7FF => p.push(format!("{name}: app_id {id:#X} is outside 0x000-0x7FF")),
                (AppKind::Raw, Some(id)) if !matches!(app.user_application(), decdrm_data::UserApplication::Other(_)) => p.push(format!(
                    "{name}: app_id {id:#05X} is an application DecDRM interprets; use its type (slideshow, website, tpeg, epg or journaline)"
                )),
                (AppKind::Raw, Some(_)) => {}
                (_, Some(_)) => p.push(format!("{name}: app_id is only used with type = \"raw\"")),
                (_, None) => {}
            }
            if let Err(e) = crate::data::check_content(cfg, app) {
                p.push(format!("{name}: {e}"));
            }
        }
    }
    // --- Alternative frequencies.
    for problem in crate::afs::problems(&cfg.afs, cfg.services.len()) {
        p.push(problem);
    }

    // --- Channel simulator.
    check_simulate(cfg, &mut p);
    // Nothing below makes sense without a valid channel and services.
    p.finish()?;
    let layout = layout.expect("checked above");

    // --- Streams.
    let mut p = Problems::default();
    let requests = stream_requests(cfg, &mut p);
    if requests.len() > MAX_STREAMS {
        p.push(format!(
            "{} streams needed ({} audio, the rest data), DRM allows at most {MAX_STREAMS}; share data streams with `stream = \"name\"`",
            requests.len(),
            requests.iter().filter(|r| matches!(r.content, StreamContent::Audio { .. })).count()
        ));
    }
    let hierarchical: Vec<usize> = (0..requests.len()).filter(|&i| requests[i].hierarchical).collect();
    if msc_mode.is_hierarchical() {
        if hierarchical.len() != 1 {
            p.push(format!(
                "channel: {} needs exactly one stream marked `hierarchical = true` ({} found)",
                ch.msc_mode,
                hierarchical.len()
            ));
        }
    } else if !hierarchical.is_empty() {
        p.push(format!("`hierarchical = true` needs msc_mode HMsym or HMmix, not {}", ch.msc_mode));
    }
    p.finish()?;

    let map = CellMap::new(layout.mode, layout.occupancy).expect("valid layout");
    let mapping = msc_mode.mapping();
    // Without a part A (equal error protection) part A's level is 0, as signalled.
    let protection = MscProtection {
        part_a: if uep { usize::from(ch.protection_a) } else { 0 },
        part_b: usize::from(ch.protection_b),
        hierarchical: usize::from(ch.protection_hierarchical),
    };
    let n_mux = map.msc_cells_per_frame;
    let base = MlcParams::msc(mapping, n_mux, protection, 0);
    let vspp_bytes = base.bits_vspp / 8;

    // Order: the hierarchical stream first, then configuration order.
    let mut order: Vec<usize> = hierarchical.clone();
    order.extend((0..requests.len()).filter(|i| !hierarchical.contains(i)));

    let mut lens = vec![0usize; requests.len()];
    let mut p = Problems::default();
    for (i, r) in requests.iter().enumerate() {
        match (&r.content, r.hierarchical) {
            (StreamContent::Data { packet_len, .. }, true) => {
                lens[i] = vspp_bytes / packet_len * packet_len;
                if lens[i] == 0 {
                    p.push(format!(
                        "the hierarchical part holds {vspp_bytes} bytes per frame, less than one {packet_len}-byte packet"
                    ));
                }
            }
            (StreamContent::Data { .. }, false) => lens[i] = r.bytes,
            (StreamContent::Audio { .. }, true) => lens[i] = vspp_bytes,
            (StreamContent::Audio { .. }, false) => {}
        }
    }
    let regular: Vec<usize> = (0..requests.len()).filter(|&i| !requests[i].hierarchical).collect();
    let audio_idx: Vec<usize> =
        regular.iter().copied().filter(|&i| matches!(requests[i].content, StreamContent::Audio { .. })).collect();
    let share_sum: f64 = audio_idx.iter().map(|&i| requests[i].share).sum();
    // Lengths with `total` audio bytes, and the MLC parameters if they fit.
    let try_total = |total: usize, lens: &mut Vec<usize>| -> Option<MlcParams> {
        let mut assigned = 0;
        for &i in &audio_idx {
            lens[i] = (total as f64 * requests[i].share / share_sum).floor() as usize;
            assigned += lens[i];
        }
        if let Some(&first) = audio_idx.first() {
            lens[first] += total - assigned;
        }
        let part_a: usize = regular.iter().filter(|&&i| requests[i].part == Part::A).map(|&i| lens[i]).sum();
        let params = MlcParams::msc(mapping, n_mux, protection, part_a);
        if part_a > 0 && params.n1 == 0 {
            return None;
        }
        let need = 8 * regular.iter().map(|&i| lens[i]).sum::<usize>();
        (need <= params.bits_hpp + params.bits_lpp && 8 * part_a <= params.bits_hpp).then_some(params)
    };
    let data_bytes: usize = regular.iter().filter(|i| !audio_idx.contains(i)).map(|&i| lens[i]).sum();
    let main_bytes = (base.bits_hpp + base.bits_lpp) / 8;
    let Some(params) = try_total(0, &mut lens) else {
        p.push(format!(
            "the data applications need {data_bytes} bytes per frame ({:.1} kbit/s) but the MSC holds only {main_bytes} ({:.1} kbit/s){}",
            data_bytes as f64 * 8.0 / FRAME_SECONDS / 1000.0,
            main_bytes as f64 * 8.0 / FRAME_SECONDS / 1000.0,
            if requests.iter().any(|r| r.part == Part::A) { " with this part A length" } else { "" }
        ));
        return Err(StationError::Config(ConfigProblems(p.0)));
    };
    let params = if audio_idx.is_empty() {
        params
    } else {
        // Largest total audio length that fits (bisection, see the module docs).
        let (mut lo, mut hi) = (0usize, main_bytes);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if try_total(mid, &mut lens).is_some() {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        try_total(lo, &mut lens).expect("feasible by construction")
    };
    if let Some(len) = lens.iter().copied().find(|&len| len > MAX_STREAM_BYTES) {
        p.push(format!("a stream of {len} bytes per frame exceeds the {MAX_STREAM_BYTES}-byte limit of the multiplex description"));
    }
    let part_a_bytes: usize = regular.iter().filter(|&&i| requests[i].part == Part::A).map(|&i| lens[i]).sum();

    // --- Stream plans (in stream id order).
    let mut streams = Vec::with_capacity(requests.len());
    let mut stream_of = vec![0u8; requests.len()];
    for (id, &i) in order.iter().enumerate() {
        stream_of[i] = id as u8;
        let r = &requests[i];
        let lengths = if r.hierarchical || r.part == Part::B {
            StreamLengths { part_a: 0, part_b: lens[i] }
        } else {
            StreamLengths { part_a: lens[i], part_b: 0 }
        };
        streams.push(StreamPlan { id: id as u8, lengths, hierarchical: r.hierarchical, content: r.content.clone() });
    }

    // --- Services: audio plans and applications.
    let mut services: Vec<ServicePlan> = (0..cfg.services.len())
        .map(|i| ServicePlan { short_id: i as u8, audio: None, apps: Vec::new() })
        .collect();
    for (ri, r) in requests.iter().enumerate() {
        let stream = &streams[usize::from(stream_of[ri])];
        match &r.content {
            StreamContent::Audio { service } => {
                let s = &cfg.services[*service];
                let a = s.audio.as_ref().expect("audio request");
                match audio_plan(a, stream) {
                    Ok(plan) => services[*service].audio = Some(plan),
                    Err(e) => p.push(format!("{}: {e}", service_name(cfg, *service))),
                }
            }
            StreamContent::Data { apps, .. } => {
                let total: f64 = r.app_bitrates.iter().sum();
                for (k, app) in apps.iter().enumerate() {
                    let settings = cfg.services[app.service].applications().nth(app.index).expect("valid app index");
                    services[app.service].apps.push(AppPlan {
                        kind: settings.kind,
                        user_app: settings.user_application(),
                        stream: stream.id,
                        packet_id: app.packet_id,
                        packet_length: settings.packet_length as u8,
                        bitrate: stream.bitrate() * r.app_bitrates[k] / total.max(1.0),
                    });
                }
            }
        }
    }
    p.finish()?;

    // --- Multiplex description and transmitter.
    let regular_lengths: Vec<StreamLengths> = streams.iter().filter(|s| !s.hierarchical).map(|s| s.lengths).collect();
    let multiplex = if msc_mode.is_hierarchical() {
        MultiplexDescription::new_hierarchical(
            ch.protection_a,
            ch.protection_b,
            ch.protection_hierarchical,
            streams[0].bytes(),
            &regular_lengths,
        )
    } else {
        MultiplexDescription::new(ch.protection_a, ch.protection_b, &regular_lengths)
    };
    let tx = TxConfig {
        mode: layout.mode,
        occupancy: layout.occupancy,
        msc_mode,
        sdc_mode: ch.sdc_mode.0,
        interleaving: ch.interleaving.0,
        protection,
        part_a_bytes,
        afs_index: 0,
    };
    let transmitter = Transmitter::new(tx)?;
    let capacity = transmitter.msc_capacity();
    debug_assert_eq!(capacity.hpp_bits + capacity.lpp_bits, params.bits_hpp + params.bits_lpp);
    let plan = MultiplexPlan {
        tx,
        layout,
        capacity,
        sdc_capacity: transmitter.sdc_capacity_bytes(),
        multiplex,
        streams,
        services,
        output,
        mdi: None,
    };

    // --- SDC: every entity must fit next to the multiplex description.
    let mut p = Problems::default();
    for problem in sdc::check_capacity(cfg, &plan) {
        p.push(problem);
    }
    p.finish()?;
    Ok(plan)
}

/// Checks of an audio service's settings (codec, rate, text, input).
fn check_audio(cfg: &StationConfig, name: &str, a: &crate::config::AudioSettings, p: &mut Problems) {
    let rate = a.core_rate.unwrap_or_else(|| default_core_rate(a.codec));
    if a.codec.is_aac() && !matches!(rate, 12_000 | 24_000) {
        p.push(format!("{name}: AAC core_rate {rate} Hz is not allowed (use 12000 or 24000)"));
    }
    if a.codec == Codec::Opus && rate != 48_000 {
        p.push(format!("{name}: Opus always runs at 48 kHz; remove core_rate"));
    }
    if a.codec == Codec::Dac {
        check_dac(name, a, rate, p);
    } else if a.bandwidth_kbps.is_some() {
        p.push(format!("{name}: bandwidth_kbps is only used by codec = \"dac\""));
    }
    if a.codec == Codec::XheAac {
        if a.core_rate.is_some() {
            p.push(format!("{name}: xHE-AAC has no core_rate setting; set its sampling rate with sample_rate"));
        }
        if let Some(r) = a.sample_rate
            && !XHE_AAC_SAMPLE_RATES.contains(&r)
        {
            let list: Vec<String> = XHE_AAC_SAMPLE_RATES.iter().map(u32::to_string).collect();
            p.push(format!("{name}: xHE-AAC sample_rate {r} Hz is not allowed (use {})", list.join(", ")));
        }
    } else {
        if a.sample_rate.is_some() {
            p.push(format!(
                "{name}: sample_rate is only used by codec = \"xhe-aac\"{}",
                if a.codec.is_aac() { " (AAC has core_rate)" } else { "" }
            ));
        }
        if a.sbr_ratio.is_some() {
            p.push(format!("{name}: sbr_ratio is only used by codec = \"xhe-aac\""));
        }
    }
    if !(a.share.is_finite() && a.share > 0.0) {
        p.push(format!("{name}: share must be a positive number"));
    }
    for (k, t) in a.text.iter().enumerate() {
        if t.len() > decdrm_core::mux::text::MAX_MESSAGE_BYTES {
            p.push(format!(
                "{name}: text message {k} has {} bytes; the limit is {} bytes of UTF-8",
                t.len(),
                decdrm_core::mux::text::MAX_MESSAGE_BYTES
            ));
        }
    }
    let i = &a.input;
    let sources = usize::from(i.file.is_some())
        + usize::from(i.device.is_some())
        + usize::from(i.url.is_some())
        + usize::from(i.tone_hz.is_some());
    if sources != 1 {
        p.push(format!("{name}: the audio input needs exactly one of `file`, `device`, `url` and `tone_hz`"));
    }
    if let Some(f) = &i.file {
        let path = cfg.resolve(f);
        if let Err(e) = decdrm_io::FileReader::open(&path) {
            p.push(format!("{name}: audio input: {e}"));
        }
    }
    // A web stream is checked without connecting (validation runs often, e.g. while a
    // GUI edits the configuration); the connection is made when the station starts.
    if let Some(url) = &i.url
        && let Err(e) = crate::webstream::check_url(url)
    {
        p.push(format!("{name}: audio input: {e}"));
    }
    if i.stream_titles.is_some() && i.url.is_none() {
        p.push(format!("{name}: stream_titles is only used with a web stream input (`url`)"));
    }
    let input_rate = match a.codec {
        Codec::Opus => 48_000,
        // Without sample_rate the highest rate the plan may choose; it checks the tone
        // against the rate it chooses.
        Codec::XheAac => a.sample_rate.unwrap_or(48_000),
        c if c.sbr() => 2 * rate,
        _ => rate,
    };
    if let Some(f) = i.tone_hz
        && !(f > 0.0 && f < f64::from(input_rate) / 2.0)
    {
        p.push(format!("{name}: tone_hz {f} must be between 0 and {} Hz", input_rate / 2));
    }
    if i.level_dbfs > 0.0 {
        p.push(format!("{name}: tone level_dbfs {} is above full scale", i.level_dbfs));
    }
    if !(-60.0..=40.0).contains(&i.gain_db) {
        p.push(format!("{name}: gain_db {} is outside -60..40 dB", i.gain_db));
    }
}

/// Whether any stream is in part A (unequal error protection). A hierarchical stream
/// has no part, whatever its `part` says.
fn uses_part_a(cfg: &StationConfig) -> bool {
    cfg.services.iter().any(|s| {
        s.audio.as_ref().is_some_and(|a| a.part == Part::A && !a.hierarchical)
            || s.applications().any(|app| app.part == Part::A && !app.hierarchical)
    })
}

/// Collect the streams in configuration order: per service its audio, then its
/// applications (shared streams where their name first appears).
fn stream_requests(cfg: &StationConfig, p: &mut Problems) -> Vec<Request> {
    let mut requests: Vec<Request> = Vec::new();
    let mut shared: BTreeMap<String, usize> = BTreeMap::new();
    for (si, s) in cfg.services.iter().enumerate() {
        if let Some(a) = &s.audio {
            requests.push(Request {
                content: StreamContent::Audio { service: si },
                part: a.part,
                hierarchical: a.hierarchical,
                bytes: 0,
                share: a.share,
                app_bitrates: Vec::new(),
            });
        }
        for (k, app) in s.applications().enumerate() {
            let packet_len = usize::from(app.packet_length.clamp(1, 255)) + 3;
            let r = AppRef { service: si, index: k, packet_id: app.packet_id.unwrap_or(0) };
            let idx = match &app.stream {
                Some(name) if shared.contains_key(name) => {
                    let idx = shared[name];
                    let req = &mut requests[idx];
                    if let StreamContent::Data { packet_len: pl, apps } = &mut req.content {
                        if *pl != packet_len {
                            p.push(format!(
                                "{}: stream \"{name}\" has {}-byte packets, this application asks for {}",
                                service_name(cfg, si),
                                *pl - 3,
                                packet_len - 3
                            ));
                        }
                        apps.push(r);
                    }
                    if req.part != app.part || req.hierarchical != app.hierarchical {
                        p.push(format!(
                            "{}: applications of stream \"{name}\" disagree on `part` / `hierarchical`",
                            service_name(cfg, si)
                        ));
                    }
                    idx
                }
                other => {
                    requests.push(Request {
                        content: StreamContent::Data { packet_len, apps: vec![r] },
                        part: app.part,
                        hierarchical: app.hierarchical,
                        bytes: 0,
                        share: 0.0,
                        app_bitrates: Vec::new(),
                    });
                    if let Some(name) = other {
                        shared.insert(name.clone(), requests.len() - 1);
                    }
                    requests.len() - 1
                }
            };
            requests[idx].app_bitrates.push(f64::from(app.bitrate));
        }
    }
    // Packet ids (explicit ones must differ; the others take the first free id) and
    // data lengths (whole packets for the sum of the requested bit rates).
    for r in &mut requests {
        let StreamContent::Data { packet_len, apps } = &mut r.content else { continue };
        let explicit: Vec<Option<u8>> =
            apps.iter().map(|a| cfg.services[a.service].applications().nth(a.index).and_then(|x| x.packet_id)).collect();
        let mut used = [false; 4];
        for id in explicit.iter().flatten() {
            let slot = &mut used[usize::from(*id & 3)];
            if *slot {
                p.push(format!("two applications in one stream use packet id {id}"));
            }
            *slot = true;
        }
        if apps.len() > 4 {
            p.push("more than four applications share one stream (packet ids 0-3)");
        }
        for (a, e) in apps.iter_mut().zip(&explicit) {
            a.packet_id = match e {
                Some(id) => *id,
                None => match used.iter().position(|u| !u) {
                    Some(free) => {
                        used[free] = true;
                        free as u8
                    }
                    None => 0,
                },
            };
        }
        let bits: f64 = r.app_bitrates.iter().sum();
        let bytes = bits * FRAME_SECONDS / 8.0;
        let packets = (bytes / *packet_len as f64).ceil().max(apps.len() as f64) as usize;
        r.bytes = packets * *packet_len;
    }
    requests
}

/// Whether the audio stream carries text messages: configured ones, or a web stream's
/// titles (which may come at any time, so the text bytes are always reserved).
fn has_text(a: &crate::config::AudioSettings) -> bool {
    a.text.iter().any(|t| !t.is_empty()) || a.input.sends_titles()
}

/// Codec parameters of an audio service carried in `stream`.
fn audio_plan(a: &crate::config::AudioSettings, stream: &StreamPlan) -> std::result::Result<AudioPlan, String> {
    match a.codec {
        Codec::Dac => return dac_plan(a, stream),
        Codec::XheAac => return xhe_plan(a, stream),
        _ => {}
    }
    let codec = a.codec;
    let core_rate = a.core_rate.unwrap_or_else(|| default_core_rate(codec));
    let stereo = a.stereo || codec == Codec::HeAacV2;
    let text = has_text(a);
    let len = stream.bytes();
    let super_frame_len = len.saturating_sub(if text { TEXT_MESSAGE_BYTES } else { 0 });
    let kbit = |bytes: usize| bytes as f64 * 8.0 / FRAME_SECONDS / 1000.0;
    let (frames, fmt) = match codec {
        Codec::Opus => (OPUS_FRAMES_PER_SUPER_FRAME, AacSuperFrameFormat::opus(stream.lengths)),
        _ => {
            let n = if core_rate == 12_000 { 5 } else { 10 };
            (n, AacSuperFrameFormat::aac(n, stream.lengths))
        }
    };
    let payload_len = fmt.payload_len(super_frame_len).unwrap_or(0);
    // Header, CRC and text bytes around the coded frames.
    let overhead = fmt.header_bytes() + frames + (len - super_frame_len);
    let remedy = if stream.hierarchical {
        "the hierarchical stream's length is fixed by the channel: use a higher protection_hierarchical, \
         HMsym instead of HMmix, or a wider channel"
    } else {
        "reduce the data bit rates, or use a wider channel, 64-QAM or a higher protection level number"
    };
    let (encoder_bitrate, opus_packet_bytes) = match codec {
        Codec::Opus => {
            let packet = (payload_len / OPUS_FRAMES_PER_SUPER_FRAME).min(OPUS_MAX_PACKET);
            if packet < OPUS_MIN_PACKET {
                return Err(format!(
                    "{:.1} kbit/s left for the audio stream; Opus needs at least {:.1} kbit/s ({remedy})",
                    kbit(len),
                    kbit(OPUS_MIN_PACKET * OPUS_FRAMES_PER_SUPER_FRAME + overhead)
                ));
            }
            ((packet * 8 * 50) as u32, packet)
        }
        _ => {
            let payload_rate = payload_len as f64 * 8.0 / FRAME_SECONDS;
            let bitrate = aac_encoder_bitrate(codec, core_rate, stereo, payload_rate);
            if let Err(min_rate) = bitrate {
                let min_payload_bytes = (min_rate * FRAME_SECONDS / 8.0).ceil() as usize;
                let alternative = match (codec, core_rate) {
                    (Codec::HeAac | Codec::HeAacV2, _) => "use AAC, or ",
                    (Codec::Aac, 24_000) => "use a 12 kHz core, or ",
                    _ => "",
                };
                return Err(format!(
                    "{:.1} kbit/s left for the audio stream; {} with a {} kHz core{} needs at least {:.1} kbit/s \
                     ({alternative}{remedy})",
                    kbit(len),
                    match codec {
                        Codec::Aac => "AAC",
                        Codec::HeAac => "HE-AAC",
                        _ => "HE-AAC v2",
                    },
                    core_rate / 1000,
                    if stereo && codec != Codec::HeAacV2 { " in stereo" } else { "" },
                    kbit(min_payload_bytes + overhead)
                ));
            }
            (bitrate.unwrap_or_default(), 0)
        }
    };
    let (audio_codec, mode, rate) = match codec {
        Codec::Opus => (AudioCodec::Opus, if stereo { AudioMode::Stereo } else { AudioMode::Mono }, 48_000),
        Codec::HeAacV2 => (AudioCodec::Aac, AudioMode::ParametricStereo, core_rate),
        _ => (AudioCodec::Aac, if stereo { AudioMode::Stereo } else { AudioMode::Mono }, core_rate),
    };
    let params = AudioParams::new(stream.id, audio_codec, codec.sbr(), mode, rate, text, Vec::new());
    let input_rate = match codec {
        Codec::Opus => 48_000,
        c if c.sbr() => 2 * core_rate,
        _ => core_rate,
    };
    Ok(AudioPlan {
        stream: stream.id,
        codec,
        core_rate: rate,
        stereo,
        input_rate,
        input_channels: if stereo { 2 } else { 1 },
        text,
        frames_per_super_frame: frames,
        super_frame_len,
        payload_len,
        encoder_bitrate,
        opus_packet_bytes,
        params,
        dac: None,
        xhe: None,
    })
}

/// What to do about a stream that is too short (in the error messages).
fn stream_remedy(stream: &StreamPlan) -> &'static str {
    if stream.hierarchical {
        "the hierarchical stream's length is fixed by the channel: use a higher protection_hierarchical, \
         HMsym instead of HMmix, or a wider channel"
    } else {
        "reduce the data bit rates, or use a wider channel, 64-QAM or a higher protection level number"
    }
}

/// xHE-AAC parameters of an audio service carried in `stream` (see the module docs):
/// the sampling rate, the encoder configuration — checked without, then with libxaac —
/// and SDC type 9 with the encoder's Static Config.
fn xhe_plan(a: &crate::config::AudioSettings, stream: &StreamPlan) -> std::result::Result<AudioPlan, String> {
    let text = has_text(a);
    let len = stream.bytes();
    let text_bytes = if text { TEXT_MESSAGE_BYTES } else { 0 };
    let super_frame_len = len.saturating_sub(text_bytes);
    let sbr = a.sbr_ratio.unwrap_or_default();
    let rate = a.sample_rate.unwrap_or_else(|| default_xhe_rate(super_frame_len, sbr));
    let channels: u16 = if a.stereo { 2 } else { 1 };
    let mut config = XheAacConfig::with_super_frame_bytes(rate, channels, super_frame_len);
    config.sbr = sbr.mode();
    let what = format!(
        "xHE-AAC {} at {} kHz{}",
        if a.stereo { "stereo" } else { "mono" },
        khz(rate),
        if sbr == SbrRatio::Auto { String::new() } else { format!(" with SBR ratio {sbr}") }
    );
    let kbit = |bytes: usize| bytes as f64 * 8.0 / FRAME_SECONDS / 1000.0;
    let budget = config.budget().map_err(|e| match config.min_super_frame_bytes() {
        Some(min) if super_frame_len < min => format!(
            "{:.1} kbit/s left for the audio stream; {what} needs at least {:.1} kbit/s ({})",
            kbit(len),
            kbit(min + text_bytes),
            stream_remedy(stream)
        ),
        _ => format!("{what}: {e}"),
    })?;
    if let Some(f) = a.input.tone_hz
        && f >= f64::from(rate) / 2.0
    {
        return Err(format!(
            "tone_hz {f} is not below {} Hz, half the xHE-AAC sampling rate ({} kHz{}); lower the tone or set \
             sample_rate",
            rate / 2,
            khz(rate),
            if a.sample_rate.is_none() { ", chosen for the stream's bit rate" } else { "" }
        ));
    }
    let encoder = XheAacEncoder::new(config.clone()).map_err(|e| format!("{what}: {e}"))?;
    let mode = if a.stereo { AudioMode::Stereo } else { AudioMode::Mono };
    let params =
        AudioParams::new(stream.id, AudioCodec::XheAac, false, mode, rate, text, encoder.static_config().to_vec());
    Ok(AudioPlan {
        stream: stream.id,
        codec: Codec::XheAac,
        core_rate: encoder.core_sample_rate(),
        stereo: a.stereo,
        input_rate: rate,
        input_channels: usize::from(channels),
        text,
        frames_per_super_frame: encoder.frames_per_super_frame().ceil() as usize,
        super_frame_len,
        payload_len: (budget.net_bitrate * FRAME_SECONDS / 8.0) as usize,
        encoder_bitrate: budget.net_bitrate as u32,
        opus_packet_bytes: 0,
        params,
        dac: None,
        xhe: Some(config),
    })
}

// ---------------------------------------------------------------------------------
// DAC (DecDRM's neural codec)
// ---------------------------------------------------------------------------------

/// Checks of a DAC service: 24 kHz mono, a valid bandwidth, and the codec built in
/// with its weights installed (so `decdrm tx --check` already tells).
fn check_dac(name: &str, a: &crate::config::AudioSettings, rate: u32, p: &mut Problems) {
    if rate != decdrm_dac::SAMPLE_RATE {
        p.push(format!("{name}: DAC always runs at 24 kHz; remove core_rate"));
    }
    if a.stereo {
        p.push(format!("{name}: DAC (the 24 kHz model) is mono here; remove stereo = true"));
    }
    if let Some(kbps) = a.bandwidth_kbps
        && decdrm_dac::Bandwidth::from_kbps(kbps).is_none()
    {
        p.push(format!("{name}: DAC bandwidth_kbps {kbps} is not one of 1.5, 3, 6, 12 and 24"));
    }
    if !decdrm_dac::BUILT_IN {
        p.push(format!(
            "{name}: DAC is not built into this program (build with `--features dac`, e.g. \
             cargo build --release -p decdrm-cli --features dac)"
        ));
    } else if let Err(e) = decdrm_dac::find_weights() {
        p.push(format!("{name}: {e}"));
    }
}

/// DAC parameters of an audio service carried in `stream`: the requested
/// bandwidth, or the highest that fits, and the framing that spends the spare bytes on
/// robustness (`decdrm_dac::plan`).
fn dac_plan(a: &crate::config::AudioSettings, stream: &StreamPlan) -> std::result::Result<AudioPlan, String> {
    let text = has_text(a);
    let len = stream.bytes();
    let text_bytes = if text { TEXT_MESSAGE_BYTES } else { 0 };
    let super_frame_len = len.saturating_sub(text_bytes);
    let requested = a.bandwidth_kbps.and_then(decdrm_dac::Bandwidth::from_kbps);
    let config = decdrm_dac::choose_config(super_frame_len, requested).map_err(|e| {
        let kbit = |bytes: usize| bytes as f64 * 8.0 / FRAME_SECONDS / 1000.0;
        let remedy = if stream.hierarchical {
            "the hierarchical stream's length is fixed by the channel: use a higher protection_hierarchical, \
             HMsym instead of HMmix, or a wider channel"
        } else {
            "reduce the data bit rates, or use a wider channel, 64-QAM or a higher protection level number"
        };
        let lower = if requested.is_some() { "a lower bandwidth_kbps, " } else { "" };
        format!(
            "{:.1} kbit/s left for the audio stream; DAC {} needs at least {:.1} kbit/s ({lower}{remedy})",
            kbit(len),
            e.bandwidth,
            kbit(e.needed + text_bytes)
        )
    })?;
    let rate = decdrm_dac::SAMPLE_RATE;
    let params =
        AudioParams::new(stream.id, AudioCodec::Dac, false, AudioMode::Mono, rate, text, config.codec_config().to_vec());
    Ok(AudioPlan {
        stream: stream.id,
        codec: Codec::Dac,
        core_rate: rate,
        stereo: false,
        input_rate: rate,
        input_channels: 1,
        text,
        frames_per_super_frame: decdrm_dac::FRAMES_PER_SUPER_FRAME,
        super_frame_len,
        payload_len: decdrm_dac::FrameLayout::new(config).min_bytes(),
        encoder_bitrate: config.bandwidth.bits_per_second(),
        opus_packet_bytes: 0,
        params,
        dac: Some(config),
        xhe: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chosen bit rate keeps the worst measured super frame within the safety
    /// margin, grows with the payload, and does not exist below FDK's floor.
    #[test]
    fn aac_bitrate_selection() {
        for codec in [Codec::Aac, Codec::HeAac, Codec::HeAacV2] {
            for rate in [12_000, 24_000] {
                for stereo in [false, true] {
                    let table = worst_fill(codec, rate, stereo);
                    let mut last = 0;
                    let mut min_ok = None;
                    for payload in (2_000..=80_000).step_by(500) {
                        let p = f64::from(payload);
                        match aac_encoder_bitrate(codec, rate, stereo, p) {
                            Ok(b) => {
                                assert!(worst_output(&table, f64::from(b)) <= AAC_SAFETY * p + 1.0);
                                assert!(f64::from(b) <= AAC_FILL * p + 1.0);
                                assert!(b + 1 >= last, "{codec:?} {rate} {stereo}: {b} after {last}");
                                last = b;
                                min_ok.get_or_insert(payload);
                            }
                            Err(min) => assert!(min_ok.is_none() && min > p, "{codec:?} {rate} {stereo} at {payload}"),
                        }
                    }
                    // Stereo 24 kHz AAC overshoots most: its bit rate stays well below 97 %.
                    if codec == Codec::Aac && rate == 24_000 && stereo {
                        let b = aac_encoder_bitrate(codec, rate, stereo, 12_500.0).unwrap();
                        assert!(f64::from(b) < 0.92 * 12_500.0, "{b}");
                    }
                    println!("{codec:?} {rate} stereo={stereo}: from {:?} bit/s", min_ok);
                }
            }
        }
    }
}
