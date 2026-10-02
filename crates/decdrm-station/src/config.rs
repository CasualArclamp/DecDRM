//! The station configuration: the serde model of a `station.toml` file.
//!
//! The structs mirror the file one to one (see `examples/station.toml` for an annotated
//! example). Values that are names in the file — robustness mode, modulations, codec,
//! FAC language, programme type, … — are small typed wrappers that accept several
//! spellings, case-insensitively ("64-QAM", "64qam", "QAM64"), and print a list of the
//! valid values when they do not recognise one.
//!
//! Rust notes:
//! * `#[derive(Serialize, Deserialize)]` generates the TOML reading/writing code;
//!   `#[serde(default)]` fills a missing field from `Default` (or from the named
//!   function), and `#[serde(deny_unknown_fields)]` turns a misspelt key into an error
//!   instead of silently ignoring it.
//! * The name-valued enums are read through `String` (`#[serde(try_from = "String")]`):
//!   serde first reads a string, then our `TryFrom<String>` implementation parses it and
//!   its error message ends up in the TOML error, with line and column.

use crate::afs::AfsSettings;
use crate::error::{Result, StationError};
use decdrm_core::fac::{Interleaving, LANGUAGES, MscMode, PROGRAMME_TYPES, SdcMode};
use decdrm_core::params::{RobustnessMode, SpectrumOccupancy};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::{Path, PathBuf};

/// Complete description of a DRM30 station: the channel (robustness mode, bandwidth,
/// modulation, protection), where the signal goes, the clock, and one to four services.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationConfig {
    /// Transmission parameters (FAC channel parameters, MSC protection).
    #[serde(default)]
    pub channel: ChannelSettings,
    /// Where the signal goes (file and/or sound card) and in which form.
    #[serde(default)]
    pub output: OutputSettings,
    /// SDC time and date entity.
    #[serde(default)]
    pub time: TimeSettings,
    /// Alternative frequencies (SDC types 3, 4, 7 and 11; see [`crate::afs`]).
    #[serde(default, skip_serializing_if = "AfsSettings::is_empty")]
    pub afs: AfsSettings,
    /// Channel simulator impairing the output, for testing receivers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulate: Option<SimulateSettings>,
    /// The services, in Short Id order (`[[service]]` tables in the file).
    #[serde(rename = "service", default)]
    pub services: Vec<ServiceSettings>,
    /// Modulator: transmit MDI from a content server instead of the services (the
    /// channel, services, time and alternative frequencies are then not used).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mdi: Option<MdiSettings>,
    /// Directory that relative paths are resolved against: the directory of the file
    /// the configuration was loaded from (set by [`StationConfig::load`]); `None` = the
    /// current directory.
    #[serde(skip)]
    pub base_dir: Option<PathBuf>,
}

impl StationConfig {
    /// Whether the programme ends by itself: every audio service reads a non-looping
    /// file (false for data-only stations and for tone or sound-card inputs). The same
    /// rule as [`crate::Station::inputs_finite`], available before a station (and its
    /// output file) is created.
    pub fn inputs_finite(&self) -> bool {
        if let Some(m) = &self.mdi {
            // A recording ends; UDP goes on.
            return matches!(crate::modulator::origin(m, self.base_dir.as_deref()), Ok(decdrm_mdi::source::MdiOrigin::File { .. }));
        }
        let mut inputs = self.services.iter().filter_map(|s| s.audio.as_ref()).map(|a| &a.input).peekable();
        inputs.peek().is_some() && inputs.all(|i| i.file.is_some() && !i.looped)
    }

    /// Parse a configuration from TOML text. Relative paths are resolved against the
    /// current directory unless [`Self::base_dir`] is set afterwards.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| parse_error(PathBuf::from("<station config>"), text, &e))
    }

    /// Load a configuration file; relative paths in it are resolved against the file's
    /// directory.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| StationError::Io { path: path.to_path_buf(), source })?;
        let mut cfg: Self = toml::from_str(&text).map_err(|e| parse_error(path.to_path_buf(), &text, &e))?;
        cfg.base_dir = Some(path.parent().map(Path::to_path_buf).unwrap_or_default());
        Ok(cfg)
    }

    /// The configuration as TOML text (e.g. to save what a GUI edited).
    pub fn to_toml_string(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| StationError::Parse { path: PathBuf::from("<station config>"), message: e.to_string(), location: None })
    }

    /// `path` resolved against [`Self::base_dir`] (absolute paths are kept).
    pub fn resolve(&self, path: &Path) -> PathBuf {
        match &self.base_dir {
            Some(dir) if path.is_relative() => dir.join(path),
            _ => path.to_path_buf(),
        }
    }

    /// Id of the service whose schedule an EPG application of service `owner`
    /// describes (the object's ScopeId, TS 102 371): the owner if it is an audio
    /// service, else the first audio service (a data service carrying the guide of the
    /// station's programme), and the owner in a station without audio services.
    pub fn epg_scope(&self, owner: usize) -> u32 {
        let own = &self.services[owner];
        let described = if own.is_audio() { own } else { self.services.iter().find(|s| s.is_audio()).unwrap_or(own) };
        described.id & 0xFF_FFFF
    }
}

/// A parse error with the (line, column) where the TOML parser stopped.
fn parse_error(path: PathBuf, text: &str, e: &toml::de::Error) -> StationError {
    let location = e.span().map(|span| line_column(text, span.start));
    StationError::Parse { path, message: e.to_string(), location }
}

/// Line and column (both from 1; the column counts characters) of byte `offset` in
/// `text`.
pub(crate) fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    // A span may start inside a multi-byte character; count from its first byte.
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[line_start..].chars().count() + 1)
}

// ---------------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------------

/// Transmission parameters. Defaults: mode B, 10 kHz (SO 3), 64-QAM with protection
/// level 1, 16-QAM SDC, long interleaving — Dream's defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChannelSettings {
    /// Robustness mode A–D (ES 201 980 §8.1).
    pub mode: Mode,
    /// Spectrum occupancy 0–5: 4.5, 5, 9, 10, 18 or 20 kHz. Modes C and D only exist
    /// with 3 and 5.
    pub occupancy: u8,
    /// MSC constellation: 16-QAM, 64-QAM, or 64-QAM with hierarchical mapping (HMsym,
    /// HMmix; then one stream must be marked `hierarchical`).
    pub msc_mode: MscModeSetting,
    /// SDC constellation: 4-QAM (robust, about half the capacity) or 16-QAM.
    pub sdc_mode: SdcModeSetting,
    /// MSC interleaving: long (2 s) or short (400 ms).
    pub interleaving: InterleavingSetting,
    /// Protection level of part A, the higher protected part (only used when a stream
    /// is placed in part A: unequal error protection). 0–1 for 16-QAM, 0–3 for 64-QAM;
    /// 0 is the most robust.
    pub protection_a: u8,
    /// Protection level of part B (all streams with equal error protection).
    pub protection_b: u8,
    /// Protection level of the hierarchical stream (HMsym/HMmix only), 0–3.
    pub protection_hierarchical: u8,
}

impl Default for ChannelSettings {
    fn default() -> Self {
        Self {
            mode: Mode(RobustnessMode::B),
            occupancy: 3,
            msc_mode: MscModeSetting(MscMode::Qam64Sm),
            sdc_mode: SdcModeSetting(SdcMode::Qam16),
            interleaving: InterleavingSetting(Interleaving::Long),
            protection_a: 0,
            protection_b: 1,
            protection_hierarchical: 0,
        }
    }
}

impl ChannelSettings {
    /// The spectrum occupancy, if the value is valid.
    pub fn spectrum_occupancy(&self) -> Option<SpectrumOccupancy> {
        SpectrumOccupancy::new(self.occupancy)
    }
}

// ---------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------

/// Where the transmitter signal goes. At least one of `file` and `device` must be
/// given (the CLI's `--output` replaces both).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OutputSettings {
    /// WAV or FLAC file (by extension), 48 kHz, one channel (real) or two (I/Q).
    pub file: Option<PathBuf>,
    /// Sample format of the file: int16 (default), int24 or float32 (WAV only).
    pub sample_format: SampleFormat,
    /// Sound-card output device (name or unique part of it; "default" = the system
    /// default output), e.g. a virtual cable such as "CABLE-A Input".
    pub device: Option<String>,
    /// Audio queued ahead on the sound card, milliseconds.
    pub device_buffer_ms: u32,
    /// Real IF signal or complex I/Q.
    pub format: SignalFormat,
    /// Real output: frequency of the DRM DC carrier, Hz. Default: the signal centred
    /// at 12 kHz — the DC carrier at 12 kHz for 9/10 kHz channels, ≈ 9.6–9.8 kHz for
    /// 4.5/5 kHz (all carriers above it), ≈ 7–7.3 kHz for 18/20 kHz.
    pub if_hz: Option<f64>,
    /// I/Q output: frequency of the DRM DC carrier, Hz (0 = zero IF).
    pub iq_offset_hz: f64,
    /// I/Q output: I on the right channel instead of the left.
    pub iq_swap: bool,
    /// RMS level of each output channel, dBFS (the OFDM peaks are about 10 dB higher).
    pub level_dbfs: f64,
    /// Apply the transmit channel filter (removes the OFDM side lobes).
    pub band_limit: bool,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            file: None,
            sample_format: SampleFormat::Int16,
            device: None,
            device_buffer_ms: 400,
            format: SignalFormat::Real,
            if_hz: None,
            iq_offset_hz: 0.0,
            iq_swap: false,
            level_dbfs: decdrm_core::tx::output::DEFAULT_LEVEL_DBFS,
            band_limit: true,
        }
    }
}

/// Real IF or I/Q output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SignalFormat {
    /// One real channel with the signal at `if_hz`.
    Real,
    /// Two channels, I and Q.
    Iq,
}

/// Sample format of the output file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SampleFormat {
    Int16,
    Int24,
    Float32,
}

// ---------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------

/// The channel simulator (`[simulate]`): the signal passes through a DRM channel model
/// of ES 201 980 annex B (multipath with Rayleigh fading) with a frequency offset, a
/// The modulator (`[mdi]`): the station transmits MDI (TS 102 820) from a content
/// server — a DRM multiplex made elsewhere — instead of its own services.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MdiSettings {
    /// Where the MDI comes from: a UDP port, `group:port`, `interface:group:port` or
    /// `source:interface:group:port` (Dream's syntax), or a recording (`.pcap`,
    /// `.pcapng`, `.rsA`…, raw AF/PFT).
    pub input: String,
    /// Frames queued before a sound card transmission starts, a reserve against
    /// network jitter (default 3 = 1.2 s).
    #[serde(default = "default_buffer_frames")]
    pub buffer_frames: usize,
}

fn default_buffer_frames() -> usize {
    3
}

impl MdiSettings {
    pub fn new(input: impl Into<String>) -> Self {
        Self { input: input.into(), buffer_frames: default_buffer_frames() }
    }
}

/// receiver clock error and white noise before it reaches the outputs — a test signal
/// for receivers. See [`decdrm_core::channel`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimulateSettings {
    /// DRM channel model 1–6: 1 AWGN, 2 Rice with delay, 3 US Consortium, 4 CCIR Poor,
    /// 5 and 6 (strong Doppler and delay spread, for modes C and D).
    #[serde(default = "default_channel_model")]
    pub channel: u8,
    /// Signal-to-noise ratio in the nominal channel bandwidth (as the receiver reports
    /// it), dB; none: no noise.
    #[serde(default)]
    pub snr_db: Option<f64>,
    /// Frequency offset added to the signal, Hz.
    #[serde(default)]
    pub frequency_offset_hz: f64,
    /// Clock error of the simulated receiver, ppm (positive: it samples faster). It
    /// scales the whole output spectrum, the IF too, as a sound card's clock error does.
    #[serde(default)]
    pub sample_rate_offset_ppm: f64,
    /// Seed of the fading and noise generators (the same seed gives the same signal).
    #[serde(default = "default_seed")]
    pub seed: u64,
}

fn default_channel_model() -> u8 {
    1
}

fn default_seed() -> u64 {
    1
}

impl SimulateSettings {
    /// A simulation of DRM channel `channel` at `snr_db` (none: no noise).
    pub fn new(channel: u8, snr_db: Option<f64>) -> Self {
        Self { channel, snr_db, frequency_offset_hz: 0.0, sample_rate_offset_ppm: 0.0, seed: default_seed() }
    }

    /// The simulator's settings; `None` for an invalid channel model number.
    pub fn channel_config(&self) -> Option<decdrm_core::channel::ChannelConfig> {
        Some(decdrm_core::channel::ChannelConfig {
            model: decdrm_core::channel::ChannelModel::drm(self.channel)?,
            snr_db: self.snr_db,
            freq_offset_hz: self.frequency_offset_hz,
            sample_rate_offset_ppm: self.sample_rate_offset_ppm,
            seed: self.seed,
        })
    }

    /// One-line description, e.g. `DRM channel 3 (US Consortium), SNR 15.0 dB`.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "DRM channel {} ({})",
            self.channel,
            decdrm_core::channel::ChannelModel::drm_name(self.channel)
        );
        match self.snr_db {
            Some(snr) => s += &format!(", SNR {snr:.1} dB"),
            None => s += ", no noise",
        }
        if self.frequency_offset_hz != 0.0 {
            s += &format!(", {:+.1} Hz", self.frequency_offset_hz);
        }
        if self.sample_rate_offset_ppm != 0.0 {
            s += &format!(", receiver clock {:+.0} ppm", self.sample_rate_offset_ppm);
        }
        s
    }
}

/// The SDC time and date entity (type 8), sent at the start and then once per minute
/// (ES 201 980 §6.4.3.9; Dream sends it at the minute edge too).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TimeSettings {
    /// Send the time and date entity.
    pub enabled: bool,
    /// UTC time of the first frame, ISO 8601 (`2026-09-29T18:00:00Z`); default: the
    /// system clock when the station starts. The time then advances with the signal
    /// (400 ms per frame), so a file generated faster than real time stays consistent.
    pub start: Option<String>,
    /// Local time offset to signal, minutes (rounded to half hours, ±15.5 h).
    pub local_offset_minutes: Option<i32>,
}

impl Default for TimeSettings {
    fn default() -> Self {
        Self { enabled: true, start: None, local_offset_minutes: None }
    }
}

// ---------------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------------

/// One service (`[[service]]`): an audio service (with `[service.audio]`, optionally
/// carrying data applications `[[service.app]]`), or a data service (with
/// `[service.data]` and optionally more `[[service.app]]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSettings {
    /// Label (SDC type 1), at most 16 characters.
    pub label: String,
    /// 24-bit service identifier (TOML accepts hex: `0xD0D001`).
    #[serde(alias = "service_id")]
    pub id: u32,
    /// FAC language (name from ES 201 980 table 18, e.g. "English", or 0–15).
    #[serde(default)]
    pub language: FacLanguage,
    /// ISO 639-2 language code for SDC type 12, e.g. "eng".
    #[serde(default)]
    pub iso_language: Option<String>,
    /// ISO 3166 country code for SDC type 12, e.g. "gb".
    #[serde(default)]
    pub iso_country: Option<String>,
    /// Programme type of an audio service (table 19, e.g. "Pop Music", or 0–31).
    #[serde(default)]
    pub programme_type: ProgrammeType,
    /// FAC application identifier of a data service (0 = "details in SDC type 5").
    #[serde(default)]
    pub fac_app_id: u8,
    /// The audio of an audio service.
    #[serde(default)]
    pub audio: Option<AudioSettings>,
    /// The (first) application of a data service.
    #[serde(default)]
    pub data: Option<AppSettings>,
    /// Further data applications carried with this service.
    #[serde(rename = "app", default)]
    pub apps: Vec<AppSettings>,
}

impl ServiceSettings {
    /// A service with a label and an id, and neither audio nor data yet (set `audio`
    /// or `data`).
    pub fn new(label: impl Into<String>, id: u32) -> Self {
        Self {
            label: label.into(),
            id,
            language: FacLanguage::default(),
            iso_language: None,
            iso_country: None,
            programme_type: ProgrammeType::default(),
            fac_app_id: 0,
            audio: None,
            data: None,
            apps: Vec::new(),
        }
    }

    /// Every data application of the service: `data` first, then `apps`.
    pub fn applications(&self) -> impl Iterator<Item = &AppSettings> {
        self.data.iter().chain(self.apps.iter())
    }

    /// Whether this is an audio service.
    pub fn is_audio(&self) -> bool {
        self.audio.is_some()
    }
}

/// The audio of an audio service. The stream gets whatever MSC capacity the data
/// applications leave (split between audio services by `share`), and the encoder's
/// bit rate follows from the stream length.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSettings {
    /// aac, he-aac (AAC + SBR), he-aac-v2 (AAC + SBR + parametric stereo), xhe-aac
    /// (MPEG-D USAC), opus or dac (DecDRM's neural codec extension, 24 kHz mono).
    pub codec: Codec,
    /// AAC core sampling rate: 12000 (5 frames per 400 ms) or 24000 (10 frames).
    /// Default: 24000 for AAC, 12000 for HE-AAC and HE-AAC v2. Not used by Opus
    /// (48 kHz, 20 frames of 20 ms) or xHE-AAC (see `sample_rate`).
    #[serde(default)]
    pub core_rate: Option<u32>,
    /// Stereo coding (AAC, HE-AAC, xHE-AAC, Opus); HE-AAC v2 is always parametric stereo.
    #[serde(default)]
    pub stereo: bool,
    /// xHE-AAC only: sampling rate of the coded audio — the encoder's input and the
    /// decoder's output (signalled in SDC type 9) — 9600, 12000, 16000, 19200, 24000,
    /// 32000, 38400 or 48000 Hz. Default: from the stream bit rate, 24 kHz up to
    /// 24 kbit/s, 32 kHz up to 48 kbit/s, 48 kHz above (see [`crate::plan`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// xHE-AAC only: SBR ratio — auto (default), none, 8:3, 2:1 or 4:1 (mono).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sbr_ratio: Option<SbrRatio>,
    /// Where the programme audio comes from.
    pub input: AudioInputSettings,
    /// Text messages (at most 128 bytes of UTF-8 each), sent one after the other.
    #[serde(default)]
    pub text: Vec<String>,
    /// Weight when several audio services split the remaining capacity.
    #[serde(default = "default_share")]
    pub share: f64,
    /// Protection part of the stream: B (default) or A (the higher protected part;
    /// unequal error protection).
    #[serde(default)]
    pub part: Part,
    /// Carry this stream in the very strongly protected hierarchical part (HMsym/HMmix
    /// only; its length is then fixed by the channel).
    #[serde(default)]
    pub hierarchical: bool,
    /// DAC only: bit rate of the codes, kbit/s — 1.5, 3, 6, 12 or 24. Default: the
    /// highest that fits the stream (spare bytes then carry a second copy of the most
    /// important codebooks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_kbps: Option<f64>,
}

fn default_share() -> f64 {
    1.0
}

impl AudioSettings {
    /// Audio with `codec` from `input`, everything else at its default (default core
    /// rate, mono, no text, part B).
    pub fn new(codec: Codec, input: AudioInputSettings) -> Self {
        Self {
            codec,
            core_rate: None,
            stereo: false,
            sample_rate: None,
            sbr_ratio: None,
            input,
            text: Vec::new(),
            share: 1.0,
            part: Part::B,
            hierarchical: false,
            bandwidth_kbps: None,
        }
    }
}

/// Audio source of an audio service: exactly one of `file`, `device`, `url` and
/// `tone_hz`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioInputSettings {
    /// WAV or FLAC file, any sample rate and channel count (mixed/duplicated to the
    /// coder's channels and resampled).
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// Restart the file at its end (otherwise silence follows).
    #[serde(rename = "loop", default = "default_true")]
    pub looped: bool,
    /// Sound-card input device (name or unique part of it).
    #[serde(default)]
    pub device: Option<String>,
    /// Internet radio stream to transmit (HTTP or HTTPS URL of an Icecast/Shoutcast
    /// stream or a playlist): decoded, resampled and followed in clock; reconnects when
    /// the stream drops (see [`crate::webstream`]).
    #[serde(default)]
    pub url: Option<String>,
    /// Web stream: send the stream's "now playing" titles as the service's text
    /// messages (default: yes; see [`Self::sends_titles`]). Only used with `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_titles: Option<bool>,
    /// Built-in test tone of this frequency, Hz.
    #[serde(default)]
    pub tone_hz: Option<f64>,
    /// Peak level of the test tone, dBFS.
    #[serde(default = "default_tone_level")]
    pub level_dbfs: f64,
    /// Gain applied to a file, sound-card or web stream input, dB.
    #[serde(default)]
    pub gain_db: f64,
}

fn default_true() -> bool {
    true
}

fn default_tone_level() -> f64 {
    -12.0
}

impl AudioInputSettings {
    /// A test tone input.
    pub fn tone(freq_hz: f64) -> Self {
        Self { tone_hz: Some(freq_hz), ..Self::default() }
    }

    /// A (looping) file input.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self { file: Some(path.into()), ..Self::default() }
    }

    /// A sound-card (line in) input.
    pub fn device(name: impl Into<String>) -> Self {
        Self { device: Some(name.into()), ..Self::default() }
    }

    /// A web stream input.
    pub fn url(url: impl Into<String>) -> Self {
        Self { url: Some(url.into()), ..Self::default() }
    }

    /// Whether the stream's titles go out as text messages: a web stream input with
    /// `stream_titles` not set to false. The audio stream then carries text messages
    /// even without configured ones.
    pub fn sends_titles(&self) -> bool {
        self.url.is_some() && self.stream_titles.unwrap_or(true)
    }
}

impl Default for AudioInputSettings {
    fn default() -> Self {
        Self {
            file: None,
            looped: true,
            device: None,
            url: None,
            stream_titles: None,
            tone_hz: None,
            level_dbfs: default_tone_level(),
            gain_db: 0.0,
        }
    }
}

/// A data application: slideshow, broadcast website, Journaline, EPG, TPEG or raw data,
/// in packet mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSettings {
    /// slideshow, website, journaline, epg, tpeg or raw.
    #[serde(rename = "type")]
    pub kind: AppKind,
    /// Slideshow: folder of JPEG/PNG images. Website: root directory. Journaline:
    /// page file (TOML, or JSON with a `.json` extension). EPG: optional TOML file with
    /// `[[programme]]` entries (instead of or in addition to inline ones). TPEG and raw:
    /// a file whose bytes are sent in MSC data groups, over and over.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Raw: the user application type it is signalled as (SDC type 5, DAB application
    /// domain, 0x000–0x7FF) — one that DecDRM does not interpret, so receivers capture
    /// its data.
    #[serde(default)]
    pub app_id: Option<u16>,
    /// Website: start page (default `index.html` when present).
    #[serde(default)]
    pub index: Option<String>,
    /// Requested bit rate, bit/s; rounded up to whole packets per 400 ms frame.
    #[serde(default = "default_app_bitrate")]
    pub bitrate: u32,
    /// Length of each packet's data field in bytes, 1–255 (a packet has 3 more bytes
    /// of header and CRC).
    #[serde(default = "default_packet_length")]
    pub packet_length: u16,
    /// Packet id 0–3 (default: the first one free in the stream).
    #[serde(default)]
    pub packet_id: Option<u8>,
    /// Name of a stream shared with other applications (same packet length, different
    /// packet ids); default: a stream of its own.
    #[serde(default)]
    pub stream: Option<String>,
    /// Protection part of the stream: B (default) or A.
    #[serde(default)]
    pub part: Part,
    /// Carry this stream in the hierarchical part (HMsym/HMmix only).
    #[serde(default)]
    pub hierarchical: bool,
    /// MOT segment size in bytes (slideshow, website, EPG); TPEG and raw: bytes per MSC
    /// data group (default 512).
    #[serde(default)]
    pub segment_size: Option<usize>,
    /// Journaline: deflate-compress pages. Website: gzip the MOT directory.
    #[serde(default)]
    pub compress: bool,
    /// EPG: programmes of the schedule (`[[service.data.programme]]`).
    #[serde(rename = "programme", default)]
    pub programmes: Vec<EpgProgramme>,
}

fn default_app_bitrate() -> u32 {
    2000
}

fn default_packet_length() -> u16 {
    45
}

impl AppSettings {
    /// An application of `kind` with the defaults (2 kbit/s, 45-byte packets).
    pub fn new(kind: AppKind) -> Self {
        Self {
            kind,
            path: None,
            app_id: None,
            index: None,
            bitrate: default_app_bitrate(),
            packet_length: default_packet_length(),
            packet_id: None,
            stream: None,
            part: Part::B,
            hierarchical: false,
            segment_size: None,
            compress: false,
            programmes: Vec::new(),
        }
    }

    /// The user application this application is signalled as in SDC type 5.
    pub fn user_application(&self) -> decdrm_data::UserApplication {
        match self.kind {
            AppKind::Raw => decdrm_data::UserApplication::from_id(decdrm_data::AppDomain::Dab, self.app_id.unwrap_or(0)),
            kind => kind.user_application(),
        }
    }
}

/// One programme of an EPG schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpgProgramme {
    /// Programme name (EPG mediumName, 16 characters; longer names also go into
    /// longName).
    pub title: String,
    /// Start time, ISO 8601 (`2026-09-29T18:00:00Z`, or with an offset).
    pub start: String,
    /// Duration in minutes.
    #[serde(alias = "duration")]
    pub duration_min: u32,
    /// Short description (up to 180 characters).
    #[serde(default)]
    pub description: Option<String>,
}

/// An EPG programme file: `[[programme]]` entries.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpgFile {
    #[serde(rename = "programme", default)]
    pub programmes: Vec<EpgProgramme>,
}

/// A Journaline page file: `[[page]]` entries (see [`JournalinePage`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalineFile {
    #[serde(rename = "page", default)]
    pub pages: Vec<JournalinePage>,
}

/// One Journaline page. Page 0 is the root menu. A page has at most one of `menu`
/// (links to other pages), `text` (plain text, `\n` for line breaks) and `list`
/// (rows, each a string or an array of cells); with none it is a title-only page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalinePage {
    pub id: u16,
    pub title: String,
    #[serde(default)]
    pub menu: Option<Vec<JournalineLink>>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub list: Option<Vec<JournalineRow>>,
}

/// A menu entry of a Journaline page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalineLink {
    /// Page id the entry leads to.
    pub link: u16,
    pub text: String,
}

/// A row of a Journaline list page: one text, or several cells.
///
/// Rust note: `#[serde(untagged)]` tries the variants in order, so `"x"` becomes
/// `Single` and `["a", "b"]` becomes `Cells`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JournalineRow {
    Single(String),
    Cells(Vec<String>),
}

// ---------------------------------------------------------------------------------
// Name-valued settings
// ---------------------------------------------------------------------------------

/// Lower-case `s` without separators, for lenient matching ("64-QAM" → "64qam").
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Implements `TryFrom<String>` (via a parse function) and `From<Self> for String` so
/// that `#[serde(try_from = "String", into = "String")]` works.
macro_rules! string_setting {
    ($ty:ty, $parse:expr, $print:expr) => {
        impl TryFrom<String> for $ty {
            type Error = String;
            fn try_from(s: String) -> std::result::Result<Self, String> {
                ($parse)(s.as_str())
            }
        }
        impl From<$ty> for String {
            fn from(v: $ty) -> String {
                ($print)(v)
            }
        }
        impl std::str::FromStr for $ty {
            type Err = String;
            fn from_str(s: &str) -> std::result::Result<Self, String> {
                ($parse)(s)
            }
        }
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&String::from(*self))
            }
        }
    };
}

/// Robustness mode A–D.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Mode(pub RobustnessMode);

string_setting!(
    Mode,
    |s: &str| match norm(s).as_str() {
        "a" => Ok(Mode(RobustnessMode::A)),
        "b" => Ok(Mode(RobustnessMode::B)),
        "c" => Ok(Mode(RobustnessMode::C)),
        "d" => Ok(Mode(RobustnessMode::D)),
        _ => Err(format!("unknown robustness mode \"{s}\" (use A, B, C or D)")),
    },
    |m: Mode| m.0.to_string()
);

/// MSC constellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MscModeSetting(pub MscMode);

string_setting!(
    MscModeSetting,
    |s: &str| match norm(s).as_str() {
        "16qam" | "qam16" | "16qamsm" | "16" => Ok(MscModeSetting(MscMode::Qam16Sm)),
        "64qam" | "qam64" | "64qamsm" | "64" | "sm" => Ok(MscModeSetting(MscMode::Qam64Sm)),
        "hmsym" | "64qamhmsym" | "qam64hmsym" => Ok(MscModeSetting(MscMode::Qam64HmSym)),
        "hmmix" | "64qamhmmix" | "qam64hmmix" => Ok(MscModeSetting(MscMode::Qam64HmMix)),
        _ => Err(format!("unknown MSC mode \"{s}\" (use 16-QAM, 64-QAM, HMsym or HMmix)")),
    },
    |m: MscModeSetting| match m.0 {
        MscMode::Qam16Sm => "16-QAM",
        MscMode::Qam64Sm => "64-QAM",
        MscMode::Qam64HmSym => "HMsym",
        MscMode::Qam64HmMix => "HMmix",
    }
    .to_string()
);

/// SDC constellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SdcModeSetting(pub SdcMode);

string_setting!(
    SdcModeSetting,
    |s: &str| match norm(s).as_str() {
        "4qam" | "qam4" | "qpsk" | "4" => Ok(SdcModeSetting(SdcMode::Qam4)),
        "16qam" | "qam16" | "16" => Ok(SdcModeSetting(SdcMode::Qam16)),
        _ => Err(format!("unknown SDC mode \"{s}\" (use 4-QAM or 16-QAM)")),
    },
    |m: SdcModeSetting| match m.0 {
        SdcMode::Qam4 => "4-QAM",
        SdcMode::Qam16 => "16-QAM",
    }
    .to_string()
);

/// MSC interleaving depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InterleavingSetting(pub Interleaving);

string_setting!(
    InterleavingSetting,
    |s: &str| match norm(s).as_str() {
        "long" | "2s" => Ok(InterleavingSetting(Interleaving::Long)),
        "short" | "400ms" => Ok(InterleavingSetting(Interleaving::Short)),
        _ => Err(format!("unknown interleaving \"{s}\" (use long or short)")),
    },
    |m: InterleavingSetting| match m.0 {
        Interleaving::Long => "long",
        Interleaving::Short => "short",
    }
    .to_string()
);

string_setting!(
    SignalFormat,
    |s: &str| match norm(s).as_str() {
        "real" | "if" | "realif" => Ok(SignalFormat::Real),
        "iq" | "complex" => Ok(SignalFormat::Iq),
        _ => Err(format!("unknown signal format \"{s}\" (use real or iq)")),
    },
    |m: SignalFormat| match m {
        SignalFormat::Real => "real",
        SignalFormat::Iq => "iq",
    }
    .to_string()
);

string_setting!(
    SampleFormat,
    |s: &str| match norm(s).as_str() {
        "int16" | "i16" | "16" | "pcm16" => Ok(SampleFormat::Int16),
        "int24" | "i24" | "24" | "pcm24" => Ok(SampleFormat::Int24),
        "float32" | "f32" | "float" => Ok(SampleFormat::Float32),
        _ => Err(format!("unknown sample format \"{s}\" (use int16, int24 or float32)")),
    },
    |m: SampleFormat| match m {
        SampleFormat::Int16 => "int16",
        SampleFormat::Int24 => "int24",
        SampleFormat::Float32 => "float32",
    }
    .to_string()
);

/// Audio codec of an audio service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Codec {
    /// AAC-LC.
    Aac,
    /// HE-AAC v1: AAC + SBR.
    HeAac,
    /// HE-AAC v2: AAC + SBR + parametric stereo.
    HeAacV2,
    /// xHE-AAC: MPEG-D USAC (ES 201 980 §5.3.1), mono or stereo, from about 5 kbit/s.
    XheAac,
    /// Opus (Dream's extension; not part of ES 201 980).
    Opus,
    /// DAC, the Descript Audio Codec (DecDRM's neural codec extension, 24 kHz mono,
    /// 1.5–24 kbit/s; needs the `dac` feature and the model weights).
    Dac,
}

string_setting!(
    Codec,
    |s: &str| match norm(s).as_str() {
        "aac" | "aaclc" | "lc" => Ok(Codec::Aac),
        "heaac" | "heaacv1" | "aacsbr" | "aacplus" => Ok(Codec::HeAac),
        "heaacv2" | "aacps" | "eaacplus" => Ok(Codec::HeAacV2),
        "xheaac" | "xhe" | "usac" | "xheaacusac" => Ok(Codec::XheAac),
        "opus" => Ok(Codec::Opus),
        "dac" | "descriptaudiocodec" => Ok(Codec::Dac),
        "encodec" => Err("DecDRM's neural codec is now DAC: use codec = \"dac\" (EnCodec was replaced)".to_string()),
        _ => Err(format!("unknown codec \"{s}\" (use aac, he-aac, he-aac-v2, xhe-aac, opus or dac)")),
    },
    |c: Codec| match c {
        Codec::Aac => "aac",
        Codec::HeAac => "he-aac",
        Codec::HeAacV2 => "he-aac-v2",
        Codec::XheAac => "xhe-aac",
        Codec::Opus => "opus",
        Codec::Dac => "dac",
    }
    .to_string()
);

impl Codec {
    /// Whether the codec is one of the AAC family (FDK encoder, 5/10 frames per 400 ms).
    pub fn is_aac(self) -> bool {
        matches!(self, Codec::Aac | Codec::HeAac | Codec::HeAacV2)
    }

    /// Whether the codec uses SBR signalled in SDC type 9 (HE-AAC; xHE-AAC signals its
    /// SBR in the codec config instead).
    pub fn sbr(self) -> bool {
        matches!(self, Codec::HeAac | Codec::HeAacV2)
    }
}

/// SBR ratio of an xHE-AAC service (output rate : core rate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SbrRatio {
    /// The encoder's choice for the sampling rate and bit rate
    /// (`decdrm_codecs::XheAacConfig::sbr_ratio`).
    #[default]
    Auto,
    /// No SBR: the core codes the whole band (sampling rates up to 32 kHz).
    None,
    /// 8:3 (a 768-sample core).
    Ratio8To3,
    /// 2:1.
    Ratio2To1,
    /// 4:1 (mono only; sampling rates from 32 kHz).
    Ratio4To1,
}

string_setting!(
    SbrRatio,
    |s: &str| match norm(s).as_str() {
        "auto" | "" => Ok(SbrRatio::Auto),
        "none" | "no" | "off" | "false" | "11" => Ok(SbrRatio::None),
        "83" => Ok(SbrRatio::Ratio8To3),
        "21" => Ok(SbrRatio::Ratio2To1),
        "41" => Ok(SbrRatio::Ratio4To1),
        _ => Err(format!("unknown SBR ratio \"{s}\" (use auto, none, 8:3, 2:1 or 4:1)")),
    },
    |r: SbrRatio| match r {
        SbrRatio::Auto => "auto",
        SbrRatio::None => "none",
        SbrRatio::Ratio8To3 => "8:3",
        SbrRatio::Ratio2To1 => "2:1",
        SbrRatio::Ratio4To1 => "4:1",
    }
    .to_string()
);

impl SbrRatio {
    /// The encoder setting.
    pub fn mode(self) -> decdrm_codecs::XheSbrMode {
        use decdrm_codecs::{XheSbrMode as M, XheSbrRatio as R};
        match self {
            SbrRatio::Auto => M::Auto,
            SbrRatio::None => M::Fixed(R::None),
            SbrRatio::Ratio8To3 => M::Fixed(R::Ratio8To3),
            SbrRatio::Ratio2To1 => M::Fixed(R::Ratio2To1),
            SbrRatio::Ratio4To1 => M::Fixed(R::Ratio4To1),
        }
    }
}

/// Kind of data application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum AppKind {
    /// MOT SlideShow (TS 101 499).
    Slideshow,
    /// MOT Broadcast Website (TS 101 498).
    Website,
    /// Journaline (TS 102 979).
    Journaline,
    /// Electronic Programme Guide (TS 102 818 / TS 102 371).
    Epg,
    /// TPEG traffic and travel information (user application 0x004): the bytes of a
    /// file, which DecDRM transmits and captures without interpreting them.
    Tpeg,
    /// Any other application, by its user application type (`app_id`): the bytes of a
    /// file, captured by the receiver.
    Raw,
}

string_setting!(
    AppKind,
    |s: &str| match norm(s).as_str() {
        "slideshow" | "motslideshow" | "sls" => Ok(AppKind::Slideshow),
        "website" | "bws" | "broadcastwebsite" => Ok(AppKind::Website),
        "journaline" => Ok(AppKind::Journaline),
        "epg" | "spi" => Ok(AppKind::Epg),
        "tpeg" => Ok(AppKind::Tpeg),
        "raw" | "other" => Ok(AppKind::Raw),
        _ => Err(format!(
            "unknown data application type \"{s}\" (use slideshow, website, journaline, epg, tpeg or raw)"
        )),
    },
    |k: AppKind| match k {
        AppKind::Slideshow => "slideshow",
        AppKind::Website => "website",
        AppKind::Journaline => "journaline",
        AppKind::Epg => "epg",
        AppKind::Tpeg => "tpeg",
        AppKind::Raw => "raw",
    }
    .to_string()
);

impl AppKind {
    /// The user application this kind is signalled as in SDC type 5 (for `Raw`, see
    /// [`AppSettings::user_application`]).
    pub fn user_application(self) -> decdrm_data::UserApplication {
        use decdrm_data::UserApplication as U;
        match self {
            AppKind::Slideshow => U::SlideShow,
            AppKind::Website => U::BroadcastWebsite,
            AppKind::Journaline => U::Journaline,
            AppKind::Epg => U::Epg,
            AppKind::Tpeg => U::Tpeg,
            AppKind::Raw => U::Other(0),
        }
    }

    /// The kind of a signalled user application (the reverse of
    /// [`user_application`](Self::user_application)); unknown ones are `Raw`.
    pub fn of(app: decdrm_data::UserApplication) -> Self {
        use decdrm_data::UserApplication as U;
        match app {
            U::SlideShow => AppKind::Slideshow,
            U::BroadcastWebsite => AppKind::Website,
            U::Journaline => AppKind::Journaline,
            U::Epg => AppKind::Epg,
            U::Tpeg => AppKind::Tpeg,
            U::Other(_) => AppKind::Raw,
        }
    }
}

/// Protection part of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Part {
    /// Higher protected part (`protection_a`).
    A,
    /// Lower protected part (`protection_b`).
    #[default]
    B,
}

string_setting!(
    Part,
    |s: &str| match norm(s).as_str() {
        "a" | "higher" | "high" => Ok(Part::A),
        "b" | "lower" | "low" => Ok(Part::B),
        _ => Err(format!("unknown protection part \"{s}\" (use A or B)")),
    },
    |p: Part| match p {
        Part::A => "A",
        Part::B => "B",
    }
    .to_string()
);

/// A 4-bit (language) or 5-bit (programme type) FAC code, written in the file as a
/// name from the ES 201 980 table or as a number.
fn code_from_name(what: &str, names: &[&str], s: &str) -> std::result::Result<u8, String> {
    let n = norm(s);
    if let Ok(v) = s.trim().parse::<u8>() {
        return if usize::from(v) < names.len() {
            Ok(v)
        } else {
            Err(format!("{what} {v} is out of range 0-{}", names.len() - 1))
        };
    }
    // A few convenience spellings.
    let alias = match (what, n.as_str()) {
        ("language", "none" | "unspecified" | "") => Some(0),
        ("language", "mandarin" | "chinese") => Some(3),
        ("language", "other") => Some(15),
        ("programme type", "none" | "") => Some(0),
        ("programme type", "pop") => Some(10),
        ("programme type", "rock") => Some(11),
        ("programme type", "jazz") => Some(24),
        ("programme type", "classical") => Some(14),
        _ => None,
    };
    if let Some(v) = alias {
        return Ok(v);
    }
    names.iter().position(|name| norm(name) == n && !n.starts_with("notused")).map(|i| i as u8).ok_or_else(|| {
        let valid: Vec<&str> = names.iter().copied().filter(|x| *x != "Not used").collect();
        format!("unknown {what} \"{s}\" (use a number 0-{} or one of: {})", names.len() - 1, valid.join(", "))
    })
}

/// FAC language code (ES 201 980 table 18); 0 = not specified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FacLanguage(pub u8);

/// FAC programme type code (ES 201 980 table 19); 0 = none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgrammeType(pub u8);

impl FacLanguage {
    /// Look up a language by name or number.
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        code_from_name("language", &LANGUAGES, s).map(Self)
    }

    /// The table name.
    pub fn name(self) -> &'static str {
        LANGUAGES.get(usize::from(self.0)).copied().unwrap_or("?")
    }
}

impl ProgrammeType {
    /// Look up a programme type by name or number.
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        code_from_name("programme type", &PROGRAMME_TYPES, s).map(Self)
    }

    /// The table name.
    pub fn name(self) -> &'static str {
        PROGRAMME_TYPES.get(usize::from(self.0)).copied().unwrap_or("?")
    }
}

/// Serde support for the code-or-name settings: a number or a string in the file,
/// written back as the table name.
///
/// Rust note: a serde `Visitor` is called back with whatever type the TOML parser
/// found (`visit_i64` for integers, `visit_str` for strings), which lets one field
/// accept both.
macro_rules! code_setting_serde {
    ($ty:ident, $what:literal) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.name())
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                struct V;
                impl Visitor<'_> for V {
                    type Value = $ty;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, "a {} name or number", $what)
                    }
                    fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<$ty, E> {
                        $ty::parse(&v.to_string()).map_err(E::custom)
                    }
                    fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<$ty, E> {
                        $ty::parse(&v.to_string()).map_err(E::custom)
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<$ty, E> {
                        $ty::parse(v).map_err(E::custom)
                    }
                }
                d.deserialize_any(V)
            }
        }
    };
}

code_setting_serde!(FacLanguage, "language");
code_setting_serde!(ProgrammeType, "programme type");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_errors_have_a_location() {
        // The value of `occupancy` (line 3) is missing.
        let e = StationConfig::from_toml_str("[channel]\nmode = \"B\"\noccupancy = \n").unwrap_err();
        assert_eq!(e.location().map(|(line, _)| line), Some(3), "{e}");
        assert!(matches!(e, StationError::Parse { .. }));
        let text = "ab\ncd\u{e9}\nf"; // 'é' takes bytes 5 and 6
        assert_eq!(line_column(text, 0), (1, 1));
        assert_eq!(line_column(text, 3), (2, 1));
        assert_eq!(line_column(text, 6), (2, 3), "inside a character: its start");
        assert_eq!(line_column(text, 8), (3, 1));
        assert_eq!(line_column("x", 99), (1, 2), "past the end: the end");
    }

    #[test]
    fn lenient_names() {
        assert_eq!("64-QAM".parse::<MscModeSetting>().unwrap().0, MscMode::Qam64Sm);
        assert_eq!("qam16".parse::<MscModeSetting>().unwrap().0, MscMode::Qam16Sm);
        assert_eq!("HMsym".parse::<MscModeSetting>().unwrap().0, MscMode::Qam64HmSym);
        assert_eq!("he-aac-v2".parse::<Codec>().unwrap(), Codec::HeAacV2);
        assert_eq!("HE AAC".parse::<Codec>().unwrap(), Codec::HeAac);
        assert_eq!("xHE-AAC".parse::<Codec>().unwrap(), Codec::XheAac);
        assert_eq!("usac".parse::<Codec>().unwrap(), Codec::XheAac);
        assert_eq!(Codec::XheAac.to_string(), "xhe-aac");
        assert!(!Codec::XheAac.is_aac() && !Codec::XheAac.sbr());
        assert!("mp3".parse::<Codec>().unwrap_err().contains("he-aac-v2"));
        assert_eq!("2:1".parse::<SbrRatio>().unwrap(), SbrRatio::Ratio2To1);
        assert_eq!("None".parse::<SbrRatio>().unwrap(), SbrRatio::None);
        assert_eq!(SbrRatio::Ratio8To3.to_string(), "8:3");
        assert!("3:1".parse::<SbrRatio>().unwrap_err().contains("4:1"));
        assert_eq!(FacLanguage::parse("english").unwrap(), FacLanguage(5));
        assert_eq!(FacLanguage::parse("7").unwrap(), FacLanguage(7));
        assert!(FacLanguage::parse("16").is_err());
        assert!(FacLanguage::parse("Klingon").unwrap_err().contains("English"));
        assert_eq!(ProgrammeType::parse("Pop Music").unwrap(), ProgrammeType(10));
        assert_eq!(ProgrammeType::parse("children's programmes").unwrap(), ProgrammeType(18));
        assert!(ProgrammeType::parse("not used").is_err());
    }

    #[test]
    fn toml_round_trip() {
        let text = r#"
            [channel]
            mode = "A"
            occupancy = 2
            msc_mode = "16-QAM"
            [output]
            file = "out.flac"
            [[service]]
            label = "Test"
            id = 0xABCDEF
            language = "German"
            programme_type = 11
            [service.audio]
            codec = "opus"
            input = { tone_hz = 440.0 }
            text = ["Hello"]
            [[service.app]]
            type = "epg"
            [[service.app.programme]]
            title = "News"
            start = "2026-09-29T18:00:00Z"
            duration_min = 30
        "#;
        let cfg = StationConfig::from_toml_str(text).unwrap();
        assert_eq!(cfg.channel.mode.0, RobustnessMode::A);
        assert_eq!(cfg.services[0].id, 0xABCDEF);
        assert_eq!(cfg.services[0].language, FacLanguage(7));
        assert_eq!(cfg.services[0].programme_type, ProgrammeType(11));
        assert_eq!(cfg.services[0].apps[0].programmes[0].duration_min, 30);
        let back = StationConfig::from_toml_str(&cfg.to_toml_string().unwrap()).unwrap();
        assert_eq!(back, cfg);
    }

    /// A configuration built in code (as a GUI would) validates like a parsed one.
    #[test]
    fn built_in_code() {
        let mut service = ServiceSettings::new("Code", 0x42);
        service.audio = Some(AudioSettings::new(Codec::HeAac, AudioInputSettings::tone(440.0)));
        let cfg = StationConfig {
            output: OutputSettings { device: Some("default".into()), ..Default::default() },
            services: vec![service],
            ..Default::default()
        };
        let plan = cfg.validate().unwrap();
        assert_eq!(plan.services[0].audio.as_ref().unwrap().core_rate, 12_000);
        assert_eq!(StationConfig::from_toml_str(&cfg.to_toml_string().unwrap()).unwrap().services, cfg.services);
    }

    #[test]
    fn helpful_parse_errors() {
        let e = StationConfig::from_toml_str("[channel]\nmsc_mode = \"256-QAM\"\n").unwrap_err().to_string();
        assert!(e.contains("unknown MSC mode") && e.contains("HMmix"), "{e}");
        let e = StationConfig::from_toml_str("[channel]\nmdoe = \"A\"\n").unwrap_err().to_string();
        assert!(e.contains("mdoe"), "{e}");
    }

    #[test]
    fn app_kind_from_user_application() {
        use decdrm_data::{AppDomain, UserApplication};
        for k in [AppKind::Slideshow, AppKind::Website, AppKind::Journaline, AppKind::Epg, AppKind::Tpeg] {
            assert_eq!(AppKind::of(k.user_application()), k);
            assert_eq!(AppKind::of(UserApplication::from_id(AppDomain::Dab, k.user_application().id())), k);
        }
        assert_eq!(AppKind::of(UserApplication::from_id(AppDomain::Dab, 0x123)), AppKind::Raw);
    }
}
