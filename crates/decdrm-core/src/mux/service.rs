//! The receiver's model of the tuned multiplex: services, audio and data parameters,
//! multiplex description, time and alternative frequencies, accumulated from FAC blocks
//! and SDC data entities (ES 201 980 §6.3–§6.4), plus the [`MscConfig`] the receiver
//! needs to decode the MSC.
//!
//! This replaces the SDC-related parts of Dream's `CParameter` (`Service[]`,
//! `Stream[]`, `MSCPrLe`, `AltFreqSign`, time fields) and the storing logic of
//! `CSDCReceive`.
//!
//! **Version flags / reconfiguration** (§6.4.3.0, §6.4.6): entities that use the
//! *reconfiguration* mechanism (types 0, 2, 5, 9, 10, 14) with version flag 0 describe
//! the current configuration and are applied immediately; with flag 1 they describe the
//! next configuration, which is only transmitted while the FAC reconfiguration index
//! counts down, and are held in [`NextConfiguration`] until the index returns to 0.
//! *List* entities (3, 4, 6, 7, 11, 13, 15) are collected per type; a change of the
//! flag discards the stored list. *Unique* entities (1, 8, 12) simply replace the
//! previous value. (Dream ignores the flag for types 0, 5 and 9, so it would apply the
//! next configuration's parameters too early during a reconfiguration.)

use super::sdc::{
    AfsDetailedRegion, AfsMultiplex, AfsOtherService, AfsRegion, AfsSchedule, Announcement, ApplicationInfo, AudioInfo,
    ConditionalAccess, EntityBody, FacChannelParameters, MultiplexDescription, PacketStreamFec, SdcEntity,
    ServiceLinking, StreamLengths, TimeAndDate, parse_sdc,
};
use crate::fac::{ChannelParams, Fac, LANGUAGES, PROGRAMME_TYPES, ServiceParams};
use crate::params::MAX_SERVICES;
use crate::rx::MscConfig;

// ---------------------------------------------------------------------------------
// Audio parameters (SDC type 9, interpreted)
// ---------------------------------------------------------------------------------

/// Source coding of an audio service (type 9 "audio coding"; Dream's
/// `CAudioParam::EAudCod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// 00: MPEG-4 ER AAC (optionally with SBR/PS, MPEG Surround).
    Aac,
    /// Dream's experimental Opus mode, signalled in three ways: audio coding 01
    /// (reserved in ES 201 980 V4, CELP in early versions; current Dream), AAC with
    /// sampling-rate code 7 (older Dream), and audio coding 11 without codec specific
    /// config (Dream 2.x, before xHE-AAC was assigned 11).
    Opus,
    /// 10: reserved (HVXC in early versions).
    Reserved,
    /// 11: xHE-AAC (MPEG-D USAC).
    XheAac,
    /// DecDRM's DAC (neural codec) extension: audio coding 10, reserved in ES 201 980
    /// V4, followed by a codec specific config that starts with [`DAC_CONFIG_MAGIC`]
    /// (format in the `decdrm-dac` crate). Receivers that follow the standard see a
    /// reserved coding and ignore the service.
    Dac,
    /// The EnCodec services DecDRM 0.4.6 and earlier sent: the same signalling with
    /// [`ENCODEC_CONFIG_MAGIC`]. DAC replaced EnCodec; such services are recognised and
    /// named, not decoded.
    Encodec,
}

/// Start of the codec specific config of a DAC service (after the two type 9 bytes): a
/// zero byte, then `"DAC"` and the format version digit (`'1'`).
///
/// The zero byte matters to receivers that parse the type 9 body field by field instead
/// of skipping it by its length (Dream's `CSDCReceive` reads no config for audio coding
/// 10): they read the following 7 bits as the next entity's length, see 0 — the SDC end
/// marker — and stop parsing the block cleanly. The station therefore sends a DAC type 9
/// entity last in its SDC block.
pub const DAC_CONFIG_MAGIC: [u8; 4] = [0x00, b'D', b'A', b'C'];

/// The same for the EnCodec services of DecDRM 0.4.6 and earlier.
pub const ENCODEC_CONFIG_MAGIC: [u8; 4] = [0x00, b'E', b'N', b'C'];

/// Sampling rate of the DAC model (the 24 kHz model; type 9 sampling rate code 011),
/// and of the EnCodec model before it.
pub const DAC_SAMPLE_RATE_HZ: u32 = 24_000;

/// Whether a type 9 codec specific config announces a DAC service (the magic plus at
/// least a version byte; the version itself is checked by the decoder).
pub fn is_dac_config(config: &[u8]) -> bool {
    config.len() > DAC_CONFIG_MAGIC.len() && config.starts_with(&DAC_CONFIG_MAGIC)
}

/// Whether a type 9 codec specific config announces an EnCodec service of DecDRM 0.4.6
/// or earlier.
pub fn is_encodec_config(config: &[u8]) -> bool {
    config.len() > ENCODEC_CONFIG_MAGIC.len() && config.starts_with(&ENCODEC_CONFIG_MAGIC)
}

impl AudioCodec {
    /// The 2-bit "audio coding" value.
    pub fn bits(self) -> u8 {
        match self {
            Self::Aac => 0,
            Self::Opus => 1,
            Self::Reserved | Self::Dac | Self::Encodec => 2,
            Self::XheAac => 3,
        }
    }

    pub fn from_bits(v: u8) -> Self {
        match v & 3 {
            0 => Self::Aac,
            1 => Self::Opus,
            2 => Self::Reserved,
            _ => Self::XheAac,
        }
    }
}

/// Audio mode (type 9 "audio mode").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioMode {
    Mono,
    /// AAC parametric stereo (a mono core plus PS side information in the SBR data).
    ParametricStereo,
    Stereo,
    Reserved,
}

impl AudioMode {
    pub fn bits(self) -> u8 {
        match self {
            Self::Mono => 0,
            Self::ParametricStereo => 1,
            Self::Stereo => 2,
            Self::Reserved => 3,
        }
    }

    pub fn from_bits(v: u8) -> Self {
        match v & 3 {
            0 => Self::Mono,
            1 => Self::ParametricStereo,
            2 => Self::Stereo,
            _ => Self::Reserved,
        }
    }
}

/// xHE-AAC sampling rates by 3-bit code (§6.4.3.10): the USAC output sampling rate —
/// what the decoder delivers and the encoder takes — not the core coder rate. FDK-AAC
/// reads the field that way and derives the core rate from the SBR ratio in the xHE-AAC
/// Static Config (e.g. 24 kHz with 2:1 SBR: a 12 kHz core).
pub const XHE_AAC_SAMPLE_RATES: [u32; 8] = [9_600, 12_000, 16_000, 19_200, 24_000, 32_000, 38_400, 48_000];

/// Audio parameters of one service, interpreted from SDC entity type 9 the way Dream's
/// `CAudioParam::setFromType9Bits` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioParams {
    /// Stream carrying the audio.
    pub stream_id: u8,
    pub codec: AudioCodec,
    /// SBR used (AAC only; always `false` otherwise).
    pub sbr: bool,
    pub mode: AudioMode,
    /// The signalled sampling rate in Hz. AAC: the core coder rate (12000/24000 in
    /// DRM30; the output rate doubles with SBR). xHE-AAC: the USAC output rate (see
    /// [`XHE_AAC_SAMPLE_RATES`]; the core runs at a fraction set by the SBR ratio of the
    /// Static Config). Opus: 48000. DAC: 24000.
    pub sample_rate_hz: u32,
    /// A text message occupies the last 4 bytes of each logical frame of the stream.
    pub text_flag: bool,
    pub enhancement: bool,
    /// MPEG Surround mode (3 msbs of the coder field; AAC and xHE-AAC).
    pub surround_mode: u8,
    /// xHE-AAC static configuration, or the DAC configuration ("codec specific
    /// config").
    pub codec_config: Vec<u8>,
    /// The type 9 body *after* the Short Id and Stream Id, re-encoded exactly like
    /// Dream's `CAudioParam::getType9Bytes()` — the buffer FDK-AAC's
    /// `aacDecoder_ConfigRaw` is opened with for `TT_DRM`.
    pub type9_bytes: Vec<u8>,
}

impl AudioParams {
    /// Interpret a type 9 entity. Errors are the cases in which Dream rejects the
    /// entity (reserved AAC sampling rates) and keeps the previous parameters.
    ///
    /// Deviation: Dream also rejects Opus signalled with SBR or a non-mono audio mode;
    /// that check contradicts Dream's own transmitter (which signals Opus as stereo), so
    /// Opus is accepted here with any SBR/mode bits (both are then normalised as Dream
    /// does: no SBR, stereo, 48 kHz).
    pub fn from_entity(a: &AudioInfo) -> Result<Self, &'static str> {
        let neural = match &a.codec_config {
            c if is_dac_config(c) => Some(AudioCodec::Dac),
            c if is_encodec_config(c) => Some(AudioCodec::Encodec),
            _ => None,
        };
        if let (true, Some(codec)) = (a.coding == AudioCodec::Dac.bits(), neural) {
            // DecDRM's neural codec extension (DAC, or EnCodec before it): the SBR,
            // mode, rate and coder fields are rfa for audio coding 10; the service is
            // always 24 kHz mono.
            let codec_config = a.codec_config.clone();
            let (mode, rate) = (AudioMode::Mono, DAC_SAMPLE_RATE_HZ);
            let type9_bytes = dream_type9_bytes(codec, false, mode, rate, a.text, a.enhancement, 0, &codec_config);
            return Ok(Self {
                stream_id: a.stream_id,
                codec,
                sbr: false,
                mode,
                sample_rate_hz: rate,
                text_flag: a.text,
                enhancement: a.enhancement,
                surround_mode: 0,
                codec_config,
                type9_bytes,
            });
        }
        let mut codec = AudioCodec::from_bits(a.coding);
        if codec == AudioCodec::XheAac && a.codec_config.is_empty() {
            // Dream 2.x signalled its Opus mode with audio coding 11 (reserved before
            // xHE-AAC took the value) and no codec specific config. A genuine xHE-AAC
            // entity always carries its static config (§6.4.3.10, n > 0), so an empty
            // one identifies these transmissions (verified on the Opus test recordings:
            // every packet CRC matches the 20-frame Opus framing).
            codec = AudioCodec::Opus;
        }
        let mut sbr = a.sbr;
        let mut mode = AudioMode::from_bits(a.mode);
        let rate = if codec == AudioCodec::XheAac {
            Ok(XHE_AAC_SAMPLE_RATES[usize::from(a.sample_rate & 7)])
        } else {
            match a.sample_rate & 7 {
                1 => Ok(12_000),
                3 => Ok(24_000),
                5 => Ok(48_000),
                7 => Err("sampling rate code 7"),
                _ => Err("reserved sampling rate"),
            }
        };
        let rate7 = a.sample_rate & 7 == 7 && codec == AudioCodec::Aac;
        let sample_rate_hz = if rate7 || codec == AudioCodec::Opus {
            // Dream's experimental Opus signalling ("XXX EXPERIMENTAL THIS IS NOT PART
            // OF DRM STANDARD XXX" in audioparam.cpp).
            codec = AudioCodec::Opus;
            mode = AudioMode::Stereo;
            sbr = false;
            48_000
        } else {
            rate?
        };
        if codec == AudioCodec::XheAac {
            sbr = false; // rfa for xHE-AAC
        }
        let surround_mode = a.coder_field >> 2;
        let codec_config = if codec == AudioCodec::XheAac { a.codec_config.clone() } else { Vec::new() };
        let type9_bytes =
            dream_type9_bytes(codec, sbr, mode, sample_rate_hz, a.text, a.enhancement, surround_mode, &codec_config);
        Ok(Self {
            stream_id: a.stream_id,
            codec,
            sbr,
            mode,
            sample_rate_hz,
            text_flag: a.text,
            enhancement: a.enhancement,
            surround_mode,
            codec_config,
            type9_bytes,
        })
    }

    /// Parameters for the transmitter; `type9_bytes` is filled in.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream_id: u8,
        codec: AudioCodec,
        sbr: bool,
        mode: AudioMode,
        sample_rate_hz: u32,
        text_flag: bool,
        codec_config: Vec<u8>,
    ) -> Self {
        let type9_bytes = dream_type9_bytes(codec, sbr, mode, sample_rate_hz, text_flag, false, 0, &codec_config);
        Self {
            stream_id,
            codec,
            sbr,
            mode,
            sample_rate_hz,
            text_flag,
            enhancement: false,
            surround_mode: 0,
            codec_config,
            type9_bytes,
        }
    }

    /// The type 9 entity to transmit for this service (as Dream's `CSDCTransmit` /
    /// `CAudioParam::EnqueueType9` writes it). Opus is signalled the way current Dream
    /// receivers accept it: audio coding 01, no SBR, audio mode *mono* (Dream rejects
    /// other modes, then treats the service as stereo), rate code 101. DAC goes out as
    /// audio coding 10 with rate code 011 (Dream rejects the entity for the other rate
    /// codes and would then skip the rest of the SDC block) and its config.
    pub fn to_entity(&self, short_id: u8) -> AudioInfo {
        let t = &self.type9_bytes;
        let mut b0 = t.first().copied().unwrap_or(0);
        if self.codec == AudioCodec::Opus {
            b0 = 0b0100_0101;
        }
        let b1 = t.get(1).copied().unwrap_or(0);
        AudioInfo {
            short_id,
            stream_id: self.stream_id,
            coding: b0 >> 6,
            sbr: b0 & 0x20 != 0,
            mode: (b0 >> 3) & 3,
            sample_rate: b0 & 7,
            text: b1 & 0x80 != 0,
            enhancement: b1 & 0x40 != 0,
            coder_field: (b1 >> 1) & 0x1F,
            rfa: false,
            codec_config: t.get(2..).unwrap_or(&[]).to_vec(),
        }
    }

    /// Number of AAC frames per 400 ms audio super frame in robustness modes A–D
    /// (§5.4.1): 5 at 12 kHz, 10 at 24 kHz. `None` for other rates or codecs.
    pub fn aac_frames_per_super_frame(&self) -> Option<usize> {
        match (self.codec, self.sample_rate_hz) {
            (AudioCodec::Aac, 12_000) => Some(5),
            (AudioCodec::Aac, 24_000) => Some(10),
            _ => None,
        }
    }

    /// Nominal output sampling rate of the decoder: AAC's core rate, doubled by SBR; the
    /// signalled rate itself for the other codecs (xHE-AAC signals its output rate).
    pub fn output_sample_rate_hz(&self) -> u32 {
        if self.sbr { 2 * self.sample_rate_hz } else { self.sample_rate_hz }
    }

    /// Where the SBR band starts, Hz: the core coder's Nyquist frequency (the SBR start
    /// band of the header may lie a little lower). AAC with SBR: half the core rate.
    /// xHE-AAC: the output rate times 3/16, 1/4 or 1/8 for 8:3, 2:1 or 4:1 SBR
    /// (`coreSbrFrameLengthIndexDrm` in the first bits of the Static Config, §5.3.2).
    /// `None` without SBR.
    pub fn sbr_crossover_hz(&self) -> Option<f64> {
        let fs = f64::from(self.sample_rate_hz);
        match self.codec {
            AudioCodec::Aac if self.sbr => Some(fs / 2.0),
            AudioCodec::XheAac => match self.codec_config.first()? >> 6 {
                1 => Some(fs * 3.0 / 16.0),
                2 => Some(fs / 4.0),
                3 => Some(fs / 8.0),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Dream's `CAudioParam::EnqueueType9` applied to interpreted parameters: audio coding,
/// SBR, mode, sampling rate, text and enhancement flags, coder field (MPEG Surround
/// mode for AAC/xHE-AAC, else zero), rfa = 0, then the xHE-AAC configuration (or the
/// DAC configuration, DecDRM's extension).
#[allow(clippy::too_many_arguments)]
fn dream_type9_bytes(
    codec: AudioCodec,
    sbr: bool,
    mode: AudioMode,
    rate: u32,
    text: bool,
    enhancement: bool,
    surround: u8,
    config: &[u8],
) -> Vec<u8> {
    let rate_code: u8 = match rate {
        9_600 => 0,
        12_000 => 1,
        16_000 => 2,
        19_200 => 3,
        // Dream writes nothing for AC_RESERVED at 24 kHz (code stays 0).
        24_000 => match codec {
            AudioCodec::XheAac => 4,
            AudioCodec::Aac | AudioCodec::Opus | AudioCodec::Dac | AudioCodec::Encodec => 3,
            AudioCodec::Reserved => 0,
        },
        32_000 => 5,
        38_400 => 6,
        48_000 => {
            if codec == AudioCodec::XheAac {
                7
            } else {
                5
            }
        }
        _ => 0,
    };
    let b0 = (codec.bits() << 6) | (u8::from(sbr) << 5) | (mode.bits() << 3) | rate_code;
    let coder = if matches!(codec, AudioCodec::Aac | AudioCodec::XheAac) { (surround & 7) << 2 } else { 0 };
    let b1 = (u8::from(text) << 7) | (u8::from(enhancement) << 6) | (coder << 1);
    let mut v = vec![b0, b1];
    if matches!(codec, AudioCodec::XheAac | AudioCodec::Dac | AudioCodec::Encodec) {
        v.extend_from_slice(config);
    }
    v
}

// ---------------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------------

/// Everything known about one service (Short Id 0..=3) of the tuned multiplex.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceInfo {
    pub short_id: u8,
    /// Latest FAC service parameters for this Short Id (service id, language,
    /// audio/data flag, programme type or application id, CA flags).
    pub fac: Option<ServiceParams>,
    /// Label (SDC type 1).
    pub label: Option<String>,
    /// ISO 639-2 language code (SDC type 12).
    pub language_code: Option<String>,
    /// ISO 3166 country code (SDC type 12).
    pub country_code: Option<String>,
    /// Audio parameters of the current configuration (SDC type 9).
    pub audio: Option<AudioParams>,
    /// Data services / applications of the current configuration (SDC type 5); an
    /// audio service may carry several.
    pub applications: Vec<ApplicationInfo>,
    /// Conditional access parameters (SDC type 2).
    pub conditional_access: Vec<ConditionalAccess>,
}

impl ServiceInfo {
    fn new(short_id: u8) -> Self {
        Self { short_id, ..Default::default() }
    }

    /// 24-bit service identifier from the FAC.
    pub fn service_id(&self) -> Option<u32> {
        self.fac.map(|f| f.service_id)
    }

    /// Audio service according to the FAC.
    pub fn is_audio(&self) -> bool {
        self.fac.is_some_and(|f| !f.is_data)
    }

    pub fn is_data(&self) -> bool {
        self.fac.is_some_and(|f| f.is_data)
    }

    /// FAC language (table 18).
    pub fn fac_language(&self) -> Option<&'static str> {
        self.fac.map(|f| LANGUAGES[usize::from(f.language & 15)])
    }

    /// Programme type of an audio service (table 19).
    pub fn programme_type(&self) -> Option<&'static str> {
        self.fac.filter(|f| !f.is_data).map(|f| PROGRAMME_TYPES[usize::from(f.descriptor & 31)])
    }

    /// Application identifier of a data service (FAC service descriptor).
    pub fn application_id(&self) -> Option<u8> {
        self.fac.filter(|f| f.is_data).map(|f| f.descriptor)
    }

    /// FAC service descriptor 30: a Warning/Alarm announcement is active (§6.3.4).
    pub fn alarm(&self) -> bool {
        self.fac.is_some_and(|f| f.descriptor == 30)
    }

    /// Stream carrying the audio of this service.
    pub fn audio_stream(&self) -> Option<u8> {
        self.audio.as_ref().map(|a| a.stream_id)
    }

    fn clear_sdc(&mut self) {
        *self = Self { short_id: self.short_id, fac: self.fac, ..Default::default() };
    }
}

// ---------------------------------------------------------------------------------
// Lists (AFS, announcements, linking)
// ---------------------------------------------------------------------------------

/// Entities of one type managed with the *list* version mechanism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityList<T> {
    version: Option<bool>,
    items: Vec<T>,
}

// `#[derive(Default)]` would require `T: Default`; an empty list needs no such bound.
impl<T> Default for EntityList<T> {
    fn default() -> Self {
        Self { version: None, items: Vec::new() }
    }
}

impl<T: PartialEq> EntityList<T> {
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// Version flag of the stored list.
    pub fn version(&self) -> Option<bool> {
        self.version
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Add `item` received with `version`; a new version discards the old list.
    /// Returns whether the list changed.
    pub fn insert(&mut self, version: bool, item: T) -> bool {
        self.insert_by(version, item, |a, b| a == b)
    }

    /// Like [`Self::insert`], but an existing item for which `same(existing, new)`
    /// holds is replaced instead of kept alongside.
    pub fn insert_by(&mut self, version: bool, item: T, same: impl Fn(&T, &T) -> bool) -> bool {
        let mut changed = false;
        if self.version != Some(version) {
            changed = self.version.is_some() && !self.items.is_empty();
            self.items.clear();
            self.version = Some(version);
        }
        match self.items.iter().position(|x| same(x, &item)) {
            Some(i) if self.items[i] == item => {}
            Some(i) => {
                self.items[i] = item;
                changed = true;
            }
            None => {
                self.items.push(item);
                changed = true;
            }
        }
        changed
    }

    pub fn clear(&mut self) {
        self.version = None;
        self.items.clear();
    }
}

/// Alternative frequency signalling (AFS) information (types 3, 4, 7, 11, 13).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AltFrequencies {
    /// Type 3: frequencies of this multiplex (or some of its services).
    pub multiplexes: EntityList<AfsMultiplex>,
    /// Type 4: schedules referenced by Schedule Id.
    pub schedules: EntityList<AfsSchedule>,
    /// Type 7: regions referenced by Region Id.
    pub regions: EntityList<AfsRegion>,
    /// Type 13: detailed regions (same Region Ids as type 7).
    pub detailed_regions: EntityList<AfsDetailedRegion>,
    /// Type 11: other services (other DRM services, AM, FM, DAB).
    pub other_services: EntityList<AfsOtherService>,
}

impl AltFrequencies {
    pub fn is_empty(&self) -> bool {
        self.multiplexes.is_empty()
            && self.schedules.is_empty()
            && self.regions.is_empty()
            && self.detailed_regions.is_empty()
            && self.other_services.is_empty()
    }

    /// Schedule definitions with the given Schedule Id.
    pub fn schedules_for(&self, schedule_id: u8) -> impl Iterator<Item = &AfsSchedule> {
        self.schedules.items().iter().filter(move |s| s.schedule_id == schedule_id)
    }

    /// Whether a frequency list with this Schedule Id is valid at `minute_of_week`
    /// (UTC minutes since Monday 00:00). Schedule Id 0 = always.
    pub fn schedule_active(&self, schedule_id: u8, minute_of_week: u32) -> bool {
        schedule_id == 0 || self.schedules_for(schedule_id).any(|s| s.is_active(minute_of_week))
    }
}

impl AfsSchedule {
    /// Whether the schedule covers `minute_of_week` (UTC minutes since Monday 00:00).
    /// Durations of a week or more cover everything (annex O allows longer ones).
    pub fn is_active(&self, minute_of_week: u32) -> bool {
        const WEEK: u32 = 7 * 1440;
        if u32::from(self.duration_minutes) >= WEEK {
            return true;
        }
        let t = minute_of_week % WEEK;
        (0..7u32).filter(|d| self.day_code & (0x40 >> d) != 0).any(|d| {
            let start = d * 1440 + u32::from(self.start_minute);
            (t + WEEK - start) % WEEK < u32::from(self.duration_minutes)
        })
    }
}

// ---------------------------------------------------------------------------------
// Ensemble
// ---------------------------------------------------------------------------------

/// Data of the next configuration, received with version flag 1 while the FAC
/// reconfiguration index is non-zero.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NextConfiguration {
    pub multiplex: Option<MultiplexDescription>,
    pub audio: [Option<AudioParams>; MAX_SERVICES],
    pub applications: [Vec<ApplicationInfo>; MAX_SERVICES],
    pub conditional_access: [Vec<ConditionalAccess>; MAX_SERVICES],
    pub packet_fec: [Option<PacketStreamFec>; 4],
    /// Type 10: channel parameters of the next configuration; `Some(None)` signals
    /// that the transmission is discontinued at the reconfiguration.
    pub channel: Option<Option<FacChannelParameters>>,
}

impl NextConfiguration {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// What changed in an [`Ensemble`] update. Per-service fields are bit masks (bit *n* =
/// Short Id *n*).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Changes {
    /// FAC channel parameters (occupancy, interleaving, MSC/SDC mode, service count).
    pub channel: bool,
    /// FAC service parameters.
    pub services: u8,
    /// Current multiplex description.
    pub multiplex: bool,
    pub labels: u8,
    pub languages: u8,
    pub audio: u8,
    pub data: u8,
    pub conditional_access: u8,
    pub time: bool,
    pub afs: bool,
    pub announcements: bool,
    pub linking: bool,
    pub packet_fec: bool,
    /// Something of the next configuration arrived.
    pub next_configuration: bool,
    /// A reconfiguration took effect (the FAC reconfiguration index returned to 0).
    pub reconfigured: bool,
}

impl Changes {
    pub fn merge(&mut self, o: Changes) {
        self.channel |= o.channel;
        self.services |= o.services;
        self.multiplex |= o.multiplex;
        self.labels |= o.labels;
        self.languages |= o.languages;
        self.audio |= o.audio;
        self.data |= o.data;
        self.conditional_access |= o.conditional_access;
        self.time |= o.time;
        self.afs |= o.afs;
        self.announcements |= o.announcements;
        self.linking |= o.linking;
        self.packet_fec |= o.packet_fec;
        self.next_configuration |= o.next_configuration;
        self.reconfigured |= o.reconfigured;
    }

    pub fn any(&self) -> bool {
        *self != Self::default()
    }

    /// The MSC decoding configuration may have changed ([`Ensemble::msc_config`]).
    pub fn msc_config(&self) -> bool {
        self.channel || self.multiplex || self.reconfigured
    }
}

/// Everything known about the tuned multiplex. Feed it every good FAC
/// ([`Ensemble::update_fac`]) and every SDC block with a good CRC
/// ([`Ensemble::update_sdc`]); call [`Ensemble::reset`] after retuning or loss of
/// synchronisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ensemble {
    channel: Option<ChannelParams>,
    services: [ServiceInfo; MAX_SERVICES],
    /// FAC block counter value when each Short Id was last signalled.
    fac_seen: [Option<u64>; MAX_SERVICES],
    fac_count: u64,
    last_reconfiguration_index: u8,
    multiplex: Option<MultiplexDescription>,
    packet_fec: [Option<PacketStreamFec>; 4],
    next: NextConfiguration,
    time: Option<TimeAndDate>,
    afs: AltFrequencies,
    announcements: EntityList<Announcement>,
    service_links: EntityList<ServiceLinking>,
}

impl Default for Ensemble {
    fn default() -> Self {
        Self::new()
    }
}

impl Ensemble {
    pub fn new() -> Self {
        Self {
            channel: None,
            // `from_fn` builds a fixed-size array by calling the closure with each index.
            services: std::array::from_fn(|i| ServiceInfo::new(i as u8)),
            fac_seen: [None; MAX_SERVICES],
            fac_count: 0,
            last_reconfiguration_index: 0,
            multiplex: None,
            packet_fec: [None; 4],
            next: NextConfiguration::default(),
            time: None,
            afs: AltFrequencies::default(),
            announcements: EntityList::default(),
            service_links: EntityList::default(),
        }
    }

    /// Forget everything (retune).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Latest FAC channel parameters.
    pub fn channel(&self) -> Option<&ChannelParams> {
        self.channel.as_ref()
    }

    /// All four service slots (including unused ones).
    pub fn service_slots(&self) -> &[ServiceInfo; MAX_SERVICES] {
        &self.services
    }

    /// Services signalled in the FAC, by Short Id. (`impl Iterator` = "some iterator
    /// type" — the caller can loop over it or call `.find()`, `.collect()`, etc.)
    pub fn services(&self) -> impl Iterator<Item = &ServiceInfo> {
        self.services.iter().filter(|s| s.fac.is_some())
    }

    pub fn service(&self, short_id: u8) -> Option<&ServiceInfo> {
        self.services.get(usize::from(short_id)).filter(|s| s.fac.is_some())
    }

    /// Current multiplex description (SDC type 0).
    pub fn multiplex(&self) -> Option<&MultiplexDescription> {
        self.multiplex.as_ref()
    }

    /// Whether the MSC uses hierarchical modulation (stream 0 is then the hierarchical
    /// stream).
    pub fn hierarchical(&self) -> bool {
        self.channel.is_some_and(|c| c.msc_mode.is_hierarchical())
    }

    /// Interpreted stream lengths of the current multiplex.
    pub fn stream_lengths(&self) -> Vec<StreamLengths> {
        self.multiplex.as_ref().map(|m| m.streams(self.hierarchical())).unwrap_or_default()
    }

    /// Packet stream FEC parameters (SDC type 14) of each stream, current configuration.
    pub fn packet_fec(&self) -> &[Option<PacketStreamFec>; 4] {
        &self.packet_fec
    }

    /// Data received for the next configuration.
    pub fn next_configuration(&self) -> &NextConfiguration {
        &self.next
    }

    /// Latest time and date (SDC type 8).
    pub fn time(&self) -> Option<&TimeAndDate> {
        self.time.as_ref()
    }

    pub fn alternative_frequencies(&self) -> &AltFrequencies {
        &self.afs
    }

    /// Announcement support and switching (SDC type 6).
    pub fn announcements(&self) -> &EntityList<Announcement> {
        &self.announcements
    }

    /// Service linking information (SDC type 15/0).
    pub fn service_links(&self) -> &EntityList<ServiceLinking> {
        &self.service_links
    }

    /// MSC decoding parameters for [`crate::rx::Receiver::set_msc_config`], once the
    /// FAC channel parameters and the multiplex description are known.
    pub fn msc_config(&self) -> Option<MscConfig> {
        // `?` on an `Option` returns `None` from the function when the value is missing.
        Some(msc_config(self.channel.as_ref()?, self.multiplex.as_ref()?))
    }

    /// Process a FAC block with a good CRC.
    pub fn update_fac(&mut self, fac: &Fac) -> Changes {
        let mut ch = Changes::default();
        self.fac_count += 1;
        let c = fac.channel;
        if let Some(old) = self.channel {
            ch.channel = old.occupancy != c.occupancy
                || old.interleaving != c.interleaving
                || old.msc_mode != c.msc_mode
                || old.sdc_mode != c.sdc_mode
                || old.num_audio != c.num_audio
                || old.num_data != c.num_data
                || old.enhancement != c.enhancement;
        } else {
            ch.channel = true;
        }
        let ri = c.reconfiguration_index;
        if self.last_reconfiguration_index != 0 && ri == 0 {
            ch.merge(self.apply_next_configuration());
        }
        self.last_reconfiguration_index = ri;
        self.channel = Some(c);

        let s = fac.service;
        let i = usize::from(s.short_id & 3);
        let svc = &mut self.services[i];
        if svc.fac.map(|f| f.service_id) != Some(s.service_id) && svc.fac.is_some() {
            // A different service now uses this Short Id: its SDC data is stale.
            svc.clear_sdc();
            ch.labels |= 1 << i;
            ch.audio |= 1 << i;
            ch.data |= 1 << i;
        }
        if svc.fac != Some(s) {
            svc.fac = Some(s);
            ch.services |= 1 << i;
        }
        self.fac_seen[i] = Some(self.fac_count);

        // More services known than the FAC announces (a false FAC CRC match, or services
        // removed by a reconfiguration): forget the least recently signalled ones. Dream
        // (`CParameter::SetNumOfServices`) resets all services in this case.
        let total = usize::from(c.num_audio + c.num_data).clamp(1, MAX_SERVICES);
        loop {
            let known: Vec<usize> = (0..MAX_SERVICES).filter(|&k| self.fac_seen[k].is_some()).collect();
            if known.len() <= total {
                break;
            }
            let oldest = *known.iter().min_by_key(|&&k| self.fac_seen[k]).expect("non-empty");
            self.fac_seen[oldest] = None;
            self.services[oldest] = ServiceInfo::new(oldest as u8);
            ch.services |= 1 << oldest;
        }
        ch
    }

    /// Parse an SDC data field (with a good CRC) and apply its entities.
    pub fn update_sdc(&mut self, data: &[u8]) -> Changes {
        let mut ch = Changes::default();
        for e in parse_sdc(data) {
            ch.merge(self.apply_entity(&e));
        }
        ch
    }

    /// Apply one SDC data entity.
    pub fn apply_entity(&mut self, e: &SdcEntity) -> Changes {
        let mut ch = Changes::default();
        let next = e.version;
        match &e.body {
            EntityBody::Multiplex(m) => {
                if next {
                    ch.next_configuration = self.next.multiplex.as_ref() != Some(m);
                    self.next.multiplex = Some(m.clone());
                } else if self.multiplex.as_ref() != Some(m) {
                    self.multiplex = Some(m.clone());
                    ch.multiplex = true;
                }
            }
            EntityBody::Label(l) => {
                let i = usize::from(l.short_id & 3);
                let text = l.text();
                if self.services[i].label.as_deref() != Some(text.as_str()) {
                    self.services[i].label = Some(text);
                    ch.labels |= 1 << i;
                }
            }
            EntityBody::ConditionalAccess(c) => {
                let i = usize::from(c.short_id & 3);
                let list =
                    if next { &mut self.next.conditional_access[i] } else { &mut self.services[i].conditional_access };
                if upsert(list, c.clone(), |a, b| a.audio_ca == b.audio_ca && a.data_ca == b.data_ca) {
                    if next {
                        ch.next_configuration = true;
                    } else {
                        ch.conditional_access |= 1 << i;
                    }
                }
            }
            EntityBody::Application(a) => {
                let i = usize::from(a.short_id & 3);
                let list = if next { &mut self.next.applications[i] } else { &mut self.services[i].applications };
                let same = |x: &ApplicationInfo, y: &ApplicationInfo| {
                    x.stream_id == y.stream_id
                        && x.packet_mode == y.packet_mode
                        && (!x.packet_mode || x.packet_id == y.packet_id)
                };
                if upsert(list, a.clone(), same) {
                    if next {
                        ch.next_configuration = true;
                    } else {
                        ch.data |= 1 << i;
                    }
                }
            }
            EntityBody::Audio(a) => {
                // Dream keeps the previous parameters when the entity is rejected.
                if let Ok(p) = AudioParams::from_entity(a) {
                    let i = usize::from(a.short_id & 3);
                    let slot = if next { &mut self.next.audio[i] } else { &mut self.services[i].audio };
                    if slot.as_ref() != Some(&p) {
                        *slot = Some(p);
                        if next {
                            ch.next_configuration = true;
                        } else {
                            ch.audio |= 1 << i;
                        }
                    }
                }
            }
            EntityBody::FacChannel(p) => {
                // Only meaningful for the next configuration; the FAC itself describes
                // the current one.
                if next && self.next.channel != Some(*p) {
                    self.next.channel = Some(*p);
                    ch.next_configuration = true;
                }
            }
            EntityBody::PacketFec(p) => {
                let slot = if next { &mut self.next.packet_fec } else { &mut self.packet_fec };
                let s = &mut slot[usize::from(p.stream_id & 3)];
                if *s != Some(*p) {
                    *s = Some(*p);
                    if next {
                        ch.next_configuration = true;
                    } else {
                        ch.packet_fec = true;
                    }
                }
            }
            EntityBody::TimeDate(t) => {
                if self.time != Some(*t) {
                    self.time = Some(*t);
                    ch.time = true;
                }
            }
            EntityBody::LanguageCountry(l) => {
                let i = usize::from(l.short_id & 3);
                let (lang, country) = (Some(l.language_str()), Some(l.country_str()));
                let s = &mut self.services[i];
                if s.language_code != lang || s.country_code != country {
                    s.language_code = lang;
                    s.country_code = country;
                    ch.languages |= 1 << i;
                }
            }
            EntityBody::AfsMultiplex(a) => ch.afs = self.afs.multiplexes.insert(e.version, a.clone()),
            EntityBody::AfsSchedule(s) => ch.afs = self.afs.schedules.insert(e.version, *s),
            EntityBody::AfsRegion(r) => ch.afs = self.afs.regions.insert(e.version, r.clone()),
            EntityBody::AfsDetailedRegion(r) => ch.afs = self.afs.detailed_regions.insert(e.version, r.clone()),
            EntityBody::AfsOtherService(o) => ch.afs = self.afs.other_services.insert(e.version, o.clone()),
            EntityBody::Announcement(a) => {
                // Only the switching flags may change without a version change.
                ch.announcements = self.announcements.insert_by(e.version, *a, |x, y| {
                    x.short_id_flags == y.short_id_flags
                        && x.other_service == y.other_service
                        && x.id == y.id
                        && x.support_flags == y.support_flags
                });
            }
            EntityBody::ServiceLinking(s) => ch.linking = self.service_links.insert(e.version, s.clone()),
            EntityBody::Unknown { .. } | EntityBody::Invalid { .. } => {}
        }
        ch
    }

    /// The reconfiguration took effect: the next configuration becomes current. Parts
    /// of it we did not receive keep their current value until the new configuration's
    /// entities (now with version flag 0) arrive — every SDC block repeats them.
    fn apply_next_configuration(&mut self) -> Changes {
        let mut ch = Changes { reconfigured: true, ..Default::default() };
        let next = std::mem::take(&mut self.next);
        if next.is_empty() {
            return ch;
        }
        if let Some(m) = next.multiplex {
            ch.multiplex = self.multiplex.as_ref() != Some(&m);
            self.multiplex = Some(m);
        }
        for (i, a) in next.audio.into_iter().enumerate() {
            if let Some(a) = a {
                if self.services[i].audio.as_ref() != Some(&a) {
                    ch.audio |= 1 << i;
                }
                self.services[i].audio = Some(a);
            }
        }
        for (i, apps) in next.applications.into_iter().enumerate() {
            if !apps.is_empty() {
                if self.services[i].applications != apps {
                    ch.data |= 1 << i;
                }
                self.services[i].applications = apps;
            }
        }
        for (i, ca) in next.conditional_access.into_iter().enumerate() {
            if !ca.is_empty() {
                ch.conditional_access |= 1 << i;
                self.services[i].conditional_access = ca;
            }
        }
        if next.packet_fec.iter().any(Option::is_some) {
            ch.packet_fec = true;
            self.packet_fec = next.packet_fec;
        }
        ch
    }
}

/// Insert or replace (by `same`) an element; returns whether the list changed.
fn upsert<T: PartialEq>(list: &mut Vec<T>, item: T, same: impl Fn(&T, &T) -> bool) -> bool {
    match list.iter().position(|x| same(x, &item)) {
        Some(i) if list[i] == item => false,
        Some(i) => {
            list[i] = item;
            true
        }
        None => {
            list.push(item);
            true
        }
    }
}

/// MSC decoding parameters from FAC channel parameters and a multiplex description:
/// MSC mode and interleaving from the FAC, protection levels and the total part A
/// length (Σ part A over all streams, the `X` of the MLC, as Dream's `CMLC` sums
/// `Stream[0..3].iLenPartA`) from the multiplex description.
pub fn msc_config(channel: &ChannelParams, mux: &MultiplexDescription) -> MscConfig {
    let hierarchical = channel.msc_mode.is_hierarchical();
    MscConfig {
        mode: channel.msc_mode,
        protection: mux.protection(hierarchical),
        part_a_bytes: mux.part_a_bytes(hierarchical),
        interleaving: channel.interleaving,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fac::{Interleaving, MscMode, SdcMode};
    use crate::fec::mlc::MscProtection;
    use crate::mux::sdc::{DrmFrequency, Label, LanguageCountry, encode_sdc_data};
    use crate::params::SpectrumOccupancy;

    /// The SBR crossover from the signalled parameters; the xHE-AAC configs are CNR-1's on
    /// 13835 kHz (8:3) and 6030 kHz (4:1).
    #[test]
    fn sbr_crossovers() {
        let p = |codec, sbr, rate, config: &[u8]| AudioParams {
            stream_id: 0,
            codec,
            sbr,
            mode: AudioMode::Mono,
            sample_rate_hz: rate,
            text_flag: false,
            enhancement: false,
            surround_mode: 0,
            codec_config: config.to_vec(),
            type9_bytes: Vec::new(),
        };
        assert_eq!(p(AudioCodec::Aac, true, 24_000, &[]).sbr_crossover_hz(), Some(12_000.0));
        assert_eq!(p(AudioCodec::Aac, false, 24_000, &[]).sbr_crossover_hz(), None);
        assert_eq!(p(AudioCodec::XheAac, false, 32_000, &[0x70, 0x9A, 0xE8]).sbr_crossover_hz(), Some(6_000.0));
        assert_eq!(p(AudioCodec::XheAac, false, 38_400, &[0xE3, 0x26, 0xEA]).sbr_crossover_hz(), Some(4_800.0));
        assert_eq!(p(AudioCodec::XheAac, false, 24_000, &[0x80]).sbr_crossover_hz(), Some(6_000.0));
        assert_eq!(p(AudioCodec::XheAac, false, 24_000, &[0x08]).sbr_crossover_hz(), None, "no SBR");
        assert_eq!(p(AudioCodec::Opus, false, 48_000, &[]).sbr_crossover_hz(), None);
    }

    fn fac(short_id: u8, service_id: u32, reconf: u8, num_audio: u8, num_data: u8, msc_mode: MscMode) -> Fac {
        Fac {
            channel: ChannelParams {
                enhancement: false,
                frame_index: 0,
                afs_valid: false,
                occupancy: SpectrumOccupancy::SO_3,
                interleaving: Interleaving::Long,
                msc_mode,
                sdc_mode: SdcMode::Qam16,
                num_audio,
                num_data,
                reconfiguration_index: reconf,
                toggle: false,
            },
            service: ServiceParams {
                service_id,
                short_id,
                audio_ca: false,
                language: 5,
                is_data: short_id == 1,
                descriptor: if short_id == 1 { 5 } else { 10 },
                data_ca: false,
            },
        }
    }

    fn audio_entity(short_id: u8, stream_id: u8, coding: u8, sbr: bool, mode: u8, rate: u8, text: bool) -> AudioInfo {
        AudioInfo { short_id, stream_id, coding, sbr, mode, sample_rate: rate, text, ..Default::default() }
    }

    #[test]
    fn type9_bytes_match_dream() {
        // AAC 24 kHz, SBR, mono, text: 00 1 00 011 | 1 0 00000 0.
        let p = AudioParams::from_entity(&audio_entity(0, 0, 0, true, 0, 3, true)).unwrap();
        assert_eq!(p.type9_bytes, vec![0x23, 0x80]);
        assert_eq!((p.codec, p.sample_rate_hz, p.mode), (AudioCodec::Aac, 24_000, AudioMode::Mono));
        assert_eq!(p.output_sample_rate_hz(), 48_000);
        assert_eq!(p.aac_frames_per_super_frame(), Some(10));
        // AAC 12 kHz PS with MPEG Surround bits and rfa set: surround kept, rfa bits
        // (coder field lsbs and the final rfa bit) are cleared as Dream re-encodes them.
        let mut e = audio_entity(1, 2, 0, true, 1, 1, false);
        e.coder_field = 0b01011;
        e.rfa = true;
        let p = AudioParams::from_entity(&e).unwrap();
        assert_eq!(p.type9_bytes, vec![0b0010_1001, 0b0001_0000]);
        assert_eq!(p.aac_frames_per_super_frame(), Some(5));
        // xHE-AAC 38.4 kHz stereo with config: rate code kept, config appended; the SBR
        // bit is rfa for xHE-AAC.
        let mut e = audio_entity(0, 1, 3, true, 2, 6, false);
        e.codec_config = vec![0x12, 0x34];
        let p = AudioParams::from_entity(&e).unwrap();
        assert_eq!(p.type9_bytes, vec![0b1101_0110, 0, 0x12, 0x34]);
        assert_eq!(p.sample_rate_hz, 38_400);
        // Opus: coding 01 (Dream-mjf), AAC + rate 7 (the "_V2" recordings) and coding 11
        // without codec config (Dream 2.x, the other Opus recordings) all normalise to
        // stereo 48 kHz.
        for e in [
            audio_entity(0, 0, 1, false, 0, 5, true),
            audio_entity(0, 0, 0, false, 0, 7, true),
            audio_entity(0, 0, 3, false, 0, 0, true),
        ] {
            let p = AudioParams::from_entity(&e).unwrap();
            assert_eq!((p.codec, p.mode, p.sample_rate_hz), (AudioCodec::Opus, AudioMode::Stereo, 48_000));
            assert_eq!(p.type9_bytes, vec![0b0101_0101, 0x80]);
        }
        // Reserved AAC sampling rates are rejected (Dream keeps the old parameters).
        assert!(AudioParams::from_entity(&audio_entity(0, 0, 0, false, 0, 2, false)).is_err());
        // Transmitter round trips.
        let p = AudioParams::new(0, AudioCodec::Aac, true, AudioMode::ParametricStereo, 12_000, true, vec![]);
        assert_eq!(AudioParams::from_entity(&p.to_entity(0)).unwrap(), p);
        let p = AudioParams::new(1, AudioCodec::XheAac, false, AudioMode::Stereo, 24_000, false, vec![0x13, 0x88]);
        assert_eq!(AudioParams::from_entity(&p.to_entity(2)).unwrap(), p);
        // Opus goes out as coding 01 / mono / no SBR, which Dream accepts.
        let p = AudioParams::new(0, AudioCodec::Opus, false, AudioMode::Stereo, 48_000, true, vec![]);
        let e = p.to_entity(0);
        assert_eq!((e.coding, e.mode, e.sbr, e.sample_rate), (1, 0, false, 5));
        assert_eq!(AudioParams::from_entity(&e).unwrap(), p);
    }

    #[test]
    fn dac_type9_signalling() {
        // DecDRM's DAC extension: audio coding 10, rate code 011, then the config.
        let config = vec![0x00, b'D', b'A', b'C', b'1', 0x48];
        let p = AudioParams::new(2, AudioCodec::Dac, false, AudioMode::Mono, 24_000, true, config.clone());
        assert_eq!(p.type9_bytes, [&[0b1000_0011, 0x80][..], &config].concat());
        assert_eq!((p.aac_frames_per_super_frame(), p.output_sample_rate_hz()), (None, 24_000));
        let e = p.to_entity(1);
        assert_eq!((e.coding, e.sbr, e.mode, e.sample_rate, e.text, e.coder_field), (2, false, 0, 3, true, 0));
        assert_eq!(e.codec_config, config);
        assert_eq!(AudioParams::from_entity(&e).unwrap(), p);
        // Through an encoded SDC data field, as a receiver sees it.
        let data = encode_sdc_data(&[SdcEntity::new(false, EntityBody::Audio(e.clone()))], 40).unwrap().data;
        let EntityBody::Audio(back) = &parse_sdc(&data)[0].body else { panic!("not an audio entity") };
        assert_eq!(AudioParams::from_entity(back).unwrap(), p);
        // A receiver that reads the type 9 body field by field without the config
        // (Dream) takes the next 7 bits as the next entity's length: 0 ends the block.
        assert_eq!(config[0] >> 1, 0);
        // Without the magic, audio coding 10 stays reserved, with the old rate checks.
        let mut r = e.clone();
        r.codec_config = vec![1, 2, 3, 4, 5];
        assert_eq!(AudioParams::from_entity(&r).unwrap().codec, AudioCodec::Reserved);
        r.codec_config.clear();
        r.sample_rate = 0;
        assert!(AudioParams::from_entity(&r).is_err());
        assert!(!is_dac_config(&DAC_CONFIG_MAGIC));
        // The EnCodec services of DecDRM 0.4.6 and earlier are recognised as such.
        let mut old = e.clone();
        old.codec_config = vec![0x00, b'E', b'N', b'C', b'1', 0x48];
        let q = AudioParams::from_entity(&old).unwrap();
        assert_eq!((q.codec, q.output_sample_rate_hz()), (AudioCodec::Encodec, 24_000));
        // Another coding with the magic is not DAC (xHE-AAC keeps its config).
        let mut x = e;
        x.coding = 3;
        assert_eq!(AudioParams::from_entity(&x).unwrap().codec, AudioCodec::XheAac);
    }

    #[test]
    fn msc_config_from_fac_and_sdc() {
        let mut ens = Ensemble::new();
        ens.update_fac(&fac(0, 0x1234, 0, 1, 1, MscMode::Qam64Sm));
        assert_eq!(ens.msc_config(), None);
        let mux = MultiplexDescription::new(
            0,
            1,
            &[StreamLengths { part_a: 10, part_b: 900 }, StreamLengths { part_a: 20, part_b: 100 }],
        );
        let data = encode_sdc_data(
            &[
                SdcEntity::new(false, EntityBody::Multiplex(mux.clone())),
                SdcEntity::new(false, EntityBody::Audio(audio_entity(0, 0, 0, true, 0, 3, true))),
                SdcEntity::new(false, EntityBody::Label(Label::new(0, "Test FM"))),
                SdcEntity::new(
                    false,
                    EntityBody::LanguageCountry(LanguageCountry { short_id: 0, language: *b"eng", country: *b"gb" }),
                ),
            ],
            97,
        )
        .unwrap()
        .data;
        let ch = ens.update_sdc(&data);
        assert!(ch.multiplex && ch.audio == 1 && ch.labels == 1 && ch.languages == 1);
        let cfg = ens.msc_config().unwrap();
        assert_eq!(cfg.part_a_bytes, 30);
        assert_eq!(cfg.protection, MscProtection { part_a: 0, part_b: 1, hierarchical: 0 });
        assert_eq!(cfg.mode, MscMode::Qam64Sm);
        let s = ens.service(0).unwrap();
        assert_eq!(s.label.as_deref(), Some("Test FM"));
        assert_eq!(s.language_code.as_deref(), Some("eng"));
        assert_eq!(s.audio_stream(), Some(0));
        assert_eq!(s.programme_type(), Some("Pop Music"));
        // Repeating the same block changes nothing.
        assert!(!ens.update_sdc(&data).any());

        // Hierarchical: stream 0's part A field holds the protection level.
        let mut ens = Ensemble::new();
        ens.update_fac(&fac(0, 1, 0, 1, 0, MscMode::Qam64HmSym));
        let mux = MultiplexDescription::new_hierarchical(1, 2, 3, 200, &[StreamLengths { part_a: 40, part_b: 500 }]);
        ens.apply_entity(&SdcEntity::new(false, EntityBody::Multiplex(mux)));
        let cfg = ens.msc_config().unwrap();
        assert_eq!(cfg.part_a_bytes, 40);
        assert_eq!(cfg.protection, MscProtection { part_a: 1, part_b: 2, hierarchical: 3 });
        assert_eq!(ens.stream_lengths()[0], StreamLengths { part_a: 0, part_b: 200 });
    }

    #[test]
    fn reconfiguration() {
        let mut ens = Ensemble::new();
        let mux_a = MultiplexDescription::new(0, 1, &[StreamLengths { part_a: 0, part_b: 1000 }]);
        let mux_b = MultiplexDescription::new(0, 0, &[StreamLengths { part_a: 0, part_b: 800 }]);
        ens.update_fac(&fac(0, 7, 0, 1, 0, MscMode::Qam64Sm));
        ens.apply_entity(&SdcEntity::new(false, EntityBody::Multiplex(mux_a.clone())));
        ens.apply_entity(&SdcEntity::new(false, EntityBody::Audio(audio_entity(0, 0, 0, true, 0, 3, false))));
        // Countdown: the next configuration arrives with version flag 1 and must not
        // affect the current one.
        ens.update_fac(&fac(0, 7, 3, 1, 0, MscMode::Qam64Sm));
        let ch = ens.apply_entity(&SdcEntity::new(true, EntityBody::Multiplex(mux_b.clone())));
        assert!(ch.next_configuration && !ch.multiplex);
        ens.apply_entity(&SdcEntity::new(true, EntityBody::Audio(audio_entity(0, 0, 0, false, 2, 3, true))));
        assert_eq!(ens.multiplex(), Some(&mux_a));
        assert!(ens.service(0).unwrap().audio.as_ref().unwrap().sbr);
        ens.update_fac(&fac(0, 7, 1, 1, 0, MscMode::Qam64Sm));
        // Index back at 0: the new configuration is active.
        let ch = ens.update_fac(&fac(0, 7, 0, 1, 0, MscMode::Qam64Sm));
        assert!(ch.reconfigured && ch.multiplex && ch.audio == 1 && ch.msc_config());
        assert_eq!(ens.multiplex(), Some(&mux_b));
        let a = ens.service(0).unwrap().audio.as_ref().unwrap();
        assert!(!a.sbr && a.text_flag && a.mode == AudioMode::Stereo);
        assert!(ens.next_configuration().multiplex.is_none());
    }

    #[test]
    fn service_changes_and_pruning() {
        let mut ens = Ensemble::new();
        ens.update_fac(&fac(0, 100, 0, 1, 1, MscMode::Qam16Sm));
        ens.update_fac(&fac(1, 200, 0, 1, 1, MscMode::Qam16Sm));
        ens.apply_entity(&SdcEntity::new(false, EntityBody::Label(Label::new(0, "Zero"))));
        assert_eq!(ens.services().count(), 2);
        // A (bogus) third service beyond the FAC's count evicts the stalest one.
        let ch = ens.update_fac(&fac(2, 300, 0, 1, 1, MscMode::Qam16Sm));
        assert_eq!(ens.services().count(), 2);
        assert!(ens.service(0).is_none() && ch.services & 1 != 0);
        // A different service id on a Short Id clears its SDC data.
        ens.update_fac(&fac(1, 200, 0, 2, 2, MscMode::Qam16Sm));
        ens.apply_entity(&SdcEntity::new(false, EntityBody::Label(Label::new(1, "One"))));
        ens.update_fac(&fac(1, 201, 0, 2, 2, MscMode::Qam16Sm));
        assert_eq!(ens.service(1).unwrap().label, None);
        assert!(ens.service(1).unwrap().is_data());
        assert_eq!(ens.service(1).unwrap().application_id(), Some(5));
    }

    #[test]
    fn list_mechanism() {
        let mut ens = Ensemble::new();
        let af = |khz: u32| AfsMultiplex { frequencies: vec![DrmFrequency::from_khz(khz)], ..Default::default() };
        assert!(ens.apply_entity(&SdcEntity::new(false, EntityBody::AfsMultiplex(af(6000)))).afs);
        assert!(ens.apply_entity(&SdcEntity::new(false, EntityBody::AfsMultiplex(af(7000)))).afs);
        assert!(!ens.apply_entity(&SdcEntity::new(false, EntityBody::AfsMultiplex(af(6000)))).afs);
        assert_eq!(ens.alternative_frequencies().multiplexes.items().len(), 2);
        // Version flip: the old list is discarded.
        assert!(ens.apply_entity(&SdcEntity::new(true, EntityBody::AfsMultiplex(af(9000)))).afs);
        assert_eq!(ens.alternative_frequencies().multiplexes.items(), &[af(9000)]);
        // Announcement switching flags replace in place.
        let ann =
            |sw: u16| Announcement { short_id_flags: 1, support_flags: 3, switching_flags: sw, ..Default::default() };
        ens.apply_entity(&SdcEntity::new(false, EntityBody::Announcement(ann(0))));
        assert!(ens.apply_entity(&SdcEntity::new(false, EntityBody::Announcement(ann(2)))).announcements);
        assert_eq!(ens.announcements().items(), &[ann(2)]);
    }

    #[test]
    fn schedules() {
        // Monday and Sunday, 23:00 for two hours.
        let s = AfsSchedule { schedule_id: 1, day_code: 0b100_0001, start_minute: 23 * 60, duration_minutes: 120 };
        let at = |day: u32, h: u32, m: u32| day * 1440 + h * 60 + m;
        assert!(s.is_active(at(0, 23, 30)));
        assert!(s.is_active(at(1, 0, 59)));
        assert!(!s.is_active(at(1, 1, 0)));
        assert!(s.is_active(at(6, 23, 0)));
        assert!(s.is_active(at(0, 0, 30))); // Sunday 23:00 + 90 min wraps into Monday
        assert!(!s.is_active(at(3, 23, 30)));
    }
}
