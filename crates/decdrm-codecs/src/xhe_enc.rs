//! xHE-AAC (MPEG-D USAC) encoder for the DRM transmitter: the libxaac encoder (through
//! `decdrm-xaac-sys`) plus the conversion of its output into DRM xHE-AAC access units and
//! the DRM rate control (ES 201 980 §5.3).
//!
//! ```no_run
//! use decdrm_codecs::{XheAacConfig, XheAacEncoder};
//! # fn run(pcm_24k_stereo: &[f32]) -> Result<(), decdrm_codecs::CodecError> {
//! // 16 kbit/s stereo: 800 bytes per 400 ms audio super frame, 24 kHz input.
//! let mut enc = XheAacEncoder::new(XheAacConfig::new(24_000, 2, 16_000))?;
//! let sdc_type9 = enc.audio_info().to_type9_bytes(); // for SDC entity 9 / the receiver
//! for au in enc.encode(pcm_24k_stereo)? {
//!     // to the super-frame builder: push_access_unit(&au.data, au.bit_reservoir_level)
//!     # let _ = (au, &sdc_type9);
//! }
//! # Ok(()) }
//! ```
//!
//! # From libxaac frames to DRM access units
//!
//! libxaac writes standard MPEG-D USAC: an AudioSpecificConfig with a UsacConfig whose
//! element 0 is an AudioPreRoll extension element, element 1 the channel element (SCE for
//! mono, CPE for stereo); every UsacFrame therefore starts with the usacIndependencyFlag and
//! the pre-roll element's "present" bit, and the first frame it outputs carries an
//! AudioPreRoll (the configuration plus the preceding frames, which the library withholds).
//!
//! DRM fixes the element list differently (§5.3.2, tables 4–7): element 0 is the channel
//! element, followed by `numExtElements` extension elements, and the configuration travels
//! in SDC entity 9 as the *xHE-AAC Static Config* rather than in the stream. FDK-AAC, the
//! decoder of DecDRM's receiver (and of most DRM receivers), even parses only element 0 of a
//! DRM USAC frame. So each frame is re-serialised as
//!
//! `usacIndependencyFlag · channel element (verbatim) · fill element · byte alignment`
//!
//! The channel element bits are copied unchanged (USAC frames have no internal byte
//! alignment, so moving an element changes nothing inside it); only the pre-roll element
//! (1 bit, or the whole AudioPreRoll of the start-up frame) is dropped. The withheld start-up
//! frames are taken from libxaac's output buffer when they are produced, so no audio is lost.
//! The exact end of the channel element comes from a small accessor added to libxaac at build
//! time (`decdrm_ixheaace_frame_bits`), because the byte-aligned frame alone does not reveal
//! it. The Static Config is built from libxaac's UsacConfig: the channel element's
//! configuration without `tw_mdct`, one `ID_EXT_ELE_FILL` extension element, no config
//! extension (libxaac's loudness info is a default value, not a measurement, and would make
//! decoders normalise the level by several dB). A unit test checks that FDK-AAC decodes the
//! DRM stream to exactly the PCM it decodes from libxaac's own MPEG stream.
//!
//! libxaac v0.1.13 needs build-time patches for DRM (see `decdrm-xaac-sys`): it accepted only
//! 64 and 96 kbit/s for USAC, rejected the 9.6/19.2 kHz core rates, and at DRM's low rates
//! coded the whole core band with a threshold in quiet below the 16-bit noise floor, which
//! wrecked the audio below ≈8 kbit/s per channel. The encoder therefore also sets the core
//! bandwidth: at most the SBR crossover, and low enough to leave ≈0.9 bit per coded
//! spectral line and channel.
//!
//! # Rate control
//!
//! An audio super frame of `L` bytes carries a 2-byte header; every audio frame costs its
//! access unit plus a 2-byte CRC and a 2-byte directory entry (§5.3.1.1–2). With frames of
//! `F` output samples at `fs` the channel therefore offers each access unit on average
//!
//! `avg = (20·(L − 2)·F − 32·fs) / fs` bits
//!
//! (the "net audio bit rate" of the spec's examples, e.g. 7 660 bit/s for 8 kbit/s mono).
//! The encoder keeps an exact ledger of the *lead* `D = Σ(access-unit bits) − n·avg` of its
//! output over the channel (exact rational arithmetic, denominator `fs`):
//!
//! * `D ≥ 0` after every frame — otherwise the super-frame builder would run out of audio
//!   frames and have to pad with bytes that corrupt the frame in progress (padding must live
//!   *inside* the audio frames, §5.3.1.3). A frame that would leave `D < 0` gets a fill
//!   element (ISO/IEC 23003-3 `ID_EXT_ELE_FILL`) of the missing size.
//! * `D ≤ R_max = 6144·channels − avg` — the MPEG bit reservoir: no frame is larger than
//!   6144 bits per channel (§5.3.1.3). libxaac runs at a slightly lower rate than `avg` with a
//!   reservoir of the same size, which keeps it inside this bound; violations (not observed
//!   in the tests) are counted in [`XheEncoderStats::overruns`].
//! * The signalled bit reservoir level is `R = R_max − D` bits, quantised as
//!   `⌊R / (384·channels)⌋` (§5.3.1.3; the decoder reconstructs `(level + 1)·384·channels`).
//! * No more than 15 frames may start in one super frame (§5.3.1.0), so no access unit is
//!   smaller than `⌈L/15⌉ − 2` bytes (a smaller frame is padded).
//!
//! The encoder starts with a full reservoir (`D = 0`), like libxaac. The super-frame builder
//! (`decdrm_core::mux::audio::XheAacFramer`, which never pads) must therefore run *ahead*
//! of the channel: before building the super frame that ends at time `t`, it must hold the
//! access units of the audio up to `t` plus about one frame; DecDRM's station encodes two
//! frames ahead (see also the xHE-AAC round-trip test for a complete transmitter loop).
//! [`XheAacConfig::budget`] checks a configuration without creating an encoder.
//!
//! # Rust / FFI notes
//!
//! * libxaac keeps pointers to nothing we own except the DRC configuration buffer, but it
//!   writes into both configuration structs on every call, so they live in `Box`es (fixed
//!   heap addresses) owned by the encoder, like the DRC buffer.
//! * `NonNull<T>` is a raw pointer that is known not to be null; the libxaac instance and its
//!   input/output buffers are held that way and freed in [`Drop`] with `ixheaace_delete`.
//! * The encoder is `Send` (it may be moved to another thread) but not `Sync`, for the same
//!   reasons as the FDK wrappers: libxaac keeps all mutable state in the memory it allocated
//!   for the instance, and its global tables are only read.

use std::ffi::{CStr, c_void};
use std::ptr::NonNull;

use decdrm_xaac_sys as ffi;

use crate::CodecError;
use crate::bits::{BitBuf, BitReader};
use crate::sdc::AudioInfo;

/// The sampling rates DRM allows for xHE-AAC (§6.4.3.10, SDC entity 9 "audio sampling
/// rate"). The encoder's input rate must be one of them; it is also the decoder's output
/// rate (the USAC sampling frequency — FDK-AAC reads the SDC field that way and derives the
/// core rate from the SBR ratio).
pub const XHE_AAC_SAMPLE_RATES: [u32; 8] = [
    9_600, 12_000, 16_000, 19_200, 24_000, 32_000, 38_400, 48_000,
];

/// Largest audio super frame for xHE-AAC: 163 920 bit/s (§5.3.1) is 8 196 bytes per 400 ms.
pub const XHE_AAC_MAX_SUPER_FRAME_BYTES: usize = 8_196;

/// Most audio frames that may start in one audio super frame (§5.3.1.0).
pub const XHE_AAC_MAX_FRAMES_PER_SUPER_FRAME: usize = 15;

/// Maximum access unit size per channel in bits (§5.3.1.3, MPEG bit reservoir).
const MAX_CHANNEL_BITS: i64 = 6_144;
/// Quantisation step of the signalled bit reservoir level, per channel (§5.3.1.3).
const BIT_RES_LEVEL_STEP: i64 = 384;
/// Smallest bit reservoir per channel DecDRM accepts (1/8 of the maximum frame).
const MIN_RESERVOIR_BITS: i64 = 768;
/// Smallest rate the (patched) libxaac accepts.
const LIBXAAC_MIN_BITRATE: i64 = 4_000;
/// Bits per coded spectral line and channel below which the core bandwidth is reduced
/// (unpatched libxaac collapsed at ≈0.6 bit per line; 0.9 keeps low-rate streams clean).
const BITS_PER_LINE: f64 = 0.9;
/// `usacExtElementType` values (ISO/IEC 23003-3 table 74).
const ID_EXT_ELE_FILL: u32 = 0;
const ID_EXT_ELE_AUDIOPREROLL: u32 = 3;
/// Fill byte of `ID_EXT_ELE_FILL` payloads ('10100101', ISO/IEC 23003-3).
const FILL_BYTE: u64 = 0xA5;

/// `usacSamplingFrequencyIndex` → Hz (ISO/IEC 23003-3 table 69; 0 = reserved).
const USAC_SAMPLING_FREQUENCIES: [u32; 31] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350, 0, 0, 57_600, 51_200, 40_000, 38_400, 34_150, 28_800, 25_600, 20_000, 19_200, 17_075,
    14_400, 12_800, 9_600, 0, 0, 0,
];

fn xaac_error(code: i32, context: &str) -> CodecError {
    CodecError::Repack(format!(
        "libxaac error {:#010x} ({}) while {context}",
        code as u32,
        ffi::ia_error_name(code)
    ))
}

/// Name and version string of the linked libxaac encoder, as the library reports them.
pub fn xaac_version() -> String {
    let mut v = ffi::ixheaace_version {
        p_lib_name: std::ptr::null_mut(),
        p_version_num: std::ptr::null_mut(),
    };
    // SAFETY: valid out-pointer; the library stores pointers to static strings.
    unsafe { ffi::ixheaace_get_lib_id_strings((&raw mut v).cast()) };
    let s = |p: *mut i8| {
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: non-null pointer to a static NUL-terminated string of the library.
            unsafe { CStr::from_ptr(p.cast()) }
                .to_string_lossy()
                .into_owned()
        }
    };
    format!("{} {}", s(v.p_lib_name), s(v.p_version_num))
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------

/// SBR ratio of the USAC configuration (`coreSbrFrameLengthIndex`, ISO/IEC 23003-3
/// table 70). DRM allows indices 1–4 (§5.3.2, `coreSbrFrameLengthIndexDrm + 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum XheSbrRatio {
    /// No SBR: 1024-sample core frames at the input rate.
    None,
    /// 8:3 SBR: 768-sample core at 3/8 of the input rate, 2048-sample output frames.
    Ratio8To3,
    /// 2:1 SBR: 1024-sample core at half the input rate, 2048-sample output frames.
    Ratio2To1,
    /// 4:1 SBR: 1024-sample core at a quarter of the input rate, 4096-sample output frames
    /// (DRM allows it for mono, or stereo with MPS212 without residual — not implemented).
    Ratio4To1,
}

impl XheSbrRatio {
    /// ISO/IEC 23003-3 `coreSbrFrameLengthIndex` (also libxaac's `ccfl_idx`).
    pub fn core_sbr_frame_length_index(self) -> u8 {
        match self {
            Self::None => 1,
            Self::Ratio8To3 => 2,
            Self::Ratio2To1 => 3,
            Self::Ratio4To1 => 4,
        }
    }

    fn from_index(index: u8) -> Option<Self> {
        match index {
            1 => Some(Self::None),
            2 => Some(Self::Ratio8To3),
            3 => Some(Self::Ratio2To1),
            4 => Some(Self::Ratio4To1),
            _ => None,
        }
    }

    /// Samples per channel of one frame at the input/output rate.
    pub fn frame_len(self) -> usize {
        match self {
            Self::None => 1024,
            Self::Ratio8To3 | Self::Ratio2To1 => 2048,
            Self::Ratio4To1 => 4096,
        }
    }

    /// Core coder sampling rate for the given input rate.
    pub fn core_rate(self, rate: u32) -> u32 {
        match self {
            Self::None => rate,
            Self::Ratio8To3 => rate * 3 / 8,
            Self::Ratio2To1 => rate / 2,
            Self::Ratio4To1 => rate / 4,
        }
    }
}

/// SBR choice of [`XheAacConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum XheSbrMode {
    /// Pick a ratio from sampling rate, channels and bit rate (see
    /// [`XheAacConfig::sbr_ratio`]).
    #[default]
    Auto,
    /// Use exactly this ratio.
    Fixed(XheSbrRatio),
}

/// Core coding mode of the USAC encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum XheCodingMode {
    /// Frequency-domain (MDCT, AAC-like) coding only — the robust default.
    #[default]
    FrequencyDomain,
    /// Switch per frame between frequency-domain and linear-prediction (ACELP/TCX) coding,
    /// libxaac's speech/music classifier deciding. Experimental in DecDRM.
    Switched,
}

/// Configuration of [`XheAacEncoder`].
#[derive(Debug, Clone, PartialEq)]
pub struct XheAacConfig {
    /// Input (and decoder output) sampling rate: one of [`XHE_AAC_SAMPLE_RATES`].
    pub sample_rate: u32,
    /// 1 (mono) or 2 (stereo, one channel pair element).
    pub channels: u16,
    /// Length of the audio super frame in bytes: the MSC stream's bytes per 400 ms logical
    /// frame, minus the 4 text-message bytes when the SDC text flag is set. The stream bit
    /// rate is `20 · super_frame_bytes` bit/s.
    pub super_frame_bytes: usize,
    /// SBR ratio.
    pub sbr: XheSbrMode,
    /// Core coding mode.
    pub coding_mode: XheCodingMode,
    /// Frames between frames with `usacIndependencyFlag = 1` (the frames a receiver can
    /// start or resume decoding at). `None`: about one per super frame, as §5.3.1.2
    /// recommends (⌊frames per super frame⌋).
    pub independent_interval: Option<u32>,
    /// USAC noise filling.
    pub noise_filling: bool,
    /// Temporal noise shaping.
    pub tns: bool,
}

impl XheAacConfig {
    /// A configuration for `bitrate` bit/s (rounded down to DRM's 20 bit/s granularity,
    /// i.e. whole bytes per 400 ms super frame).
    pub fn new(sample_rate: u32, channels: u16, bitrate: u32) -> Self {
        Self::with_super_frame_bytes(sample_rate, channels, (bitrate / 20) as usize)
    }

    /// A configuration for audio super frames of exactly `super_frame_bytes` bytes.
    pub fn with_super_frame_bytes(
        sample_rate: u32,
        channels: u16,
        super_frame_bytes: usize,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            super_frame_bytes,
            sbr: XheSbrMode::Auto,
            coding_mode: XheCodingMode::FrequencyDomain,
            independent_interval: None,
            noise_filling: true,
            tns: true,
        }
    }

    /// Stream bit rate in bit/s (`20 · super_frame_bytes`).
    pub fn bitrate(&self) -> u32 {
        (self.super_frame_bytes as u32).saturating_mul(20)
    }

    /// The SBR ratio used: the fixed one, or for [`XheSbrMode::Auto`] the one that gave the
    /// best results in DecDRM's round-trip sweep (FDK-AAC decoding every DRM rate, ratio
    /// and 8–64 kbit/s; the core should stay at ≤ 12–16 kHz at low rates):
    ///
    /// | input rate          | mono                         | stereo               |
    /// |---------------------|------------------------------|----------------------|
    /// | 9.6, 12 kHz         | no SBR                       | no SBR               |
    /// | 16, 19.2, 24, 32 kHz| 2:1                          | 2:1                  |
    /// | 38.4 kHz            | 4:1 up to 24 kbit/s          | not supported        |
    /// | 48 kHz              | 4:1 below 12 kbit/s, else 2:1| 2:1 from 12 kbit/s   |
    ///
    /// Errors when the ratio cannot be used at all: 4:1 in stereo needs MPS212 (ES 201 980
    /// §5.3.1; not implemented), libxaac runs 4:1 only at ≥ 32 kHz and has no SBR
    /// frequency tables for 38.4 kHz with 2:1 or 8:3, and without SBR above 32 kHz more
    /// than 15 frames could start in a super frame. [`XheSbrMode::Fixed`] ratios are not
    /// checked against the sweep, where these combinations decoded badly: stereo at 8–12
    /// kbit/s with an 18–32 kHz core (no SBR at 32 kHz, 8:3 or 2:1 at 48 kHz), mono 8:3 at
    /// 48 kHz and 8 kbit/s, and 4:1 at 38.4 kHz above 24 kbit/s.
    pub fn sbr_ratio(&self) -> Result<XheSbrRatio, CodecError> {
        let mono = self.channels == 1;
        let ratio = match self.sbr {
            XheSbrMode::Fixed(r) => r,
            XheSbrMode::Auto => match self.sample_rate {
                0..=12_000 => XheSbrRatio::None,
                38_400 if mono && self.bitrate() <= 24_000 => XheSbrRatio::Ratio4To1,
                38_400 => {
                    return Err(CodecError::Unsupported(format!(
                        "no usable xHE-AAC configuration at 38.4 kHz for {} at {} bit/s with \
                         libxaac (its SBR has no 38.4 kHz tables except for 4:1, which is mono \
                         only and degrades above 24 kbit/s); use a sampling rate of 24, 32 or \
                         48 kHz",
                        if mono { "mono" } else { "stereo" },
                        self.bitrate()
                    )));
                }
                48_000 if mono && self.bitrate() < 12_000 => XheSbrRatio::Ratio4To1,
                48_000 if !mono && self.bitrate() < 12_000 => {
                    return Err(CodecError::Unsupported(format!(
                        "stereo xHE-AAC at 48 kHz below 12 kbit/s decodes badly with libxaac \
                         ({} bit/s requested); use a 16 or 24 kHz sampling rate",
                        self.bitrate()
                    )));
                }
                _ => XheSbrRatio::Ratio2To1,
            },
        };
        match ratio {
            XheSbrRatio::Ratio4To1 if !mono => Err(CodecError::Unsupported(
                "xHE-AAC 4:1 SBR in stereo requires MPS212 parametric stereo (ES 201 980 \
                 §5.3.1), which DecDRM's encoder does not implement"
                    .into(),
            )),
            XheSbrRatio::Ratio4To1 if self.sample_rate < 32_000 => {
                Err(CodecError::Unsupported(format!(
                    "libxaac supports 4:1 SBR only at sampling rates of 32 kHz and above, not \
                     {} Hz",
                    self.sample_rate
                )))
            }
            XheSbrRatio::Ratio8To3 | XheSbrRatio::Ratio2To1 if self.sample_rate == 38_400 => {
                Err(CodecError::Unsupported(
                    "libxaac's SBR encoder has no frequency band tables for a 38.4 kHz output \
                     rate with 2:1 or 8:3 SBR (it rejects them at initialisation); use 4:1 \
                     (mono) or another sampling rate"
                        .into(),
                ))
            }
            XheSbrRatio::None if self.sample_rate > 32_000 => {
                Err(CodecError::Unsupported(format!(
                    "xHE-AAC without SBR at {} Hz would put more than 15 frames into a 400 ms \
                     super frame (ES 201 980 §5.3.1); use SBR",
                    self.sample_rate
                )))
            }
            r => Ok(r),
        }
    }

    fn validate(&self) -> Result<XheSbrRatio, CodecError> {
        if !XHE_AAC_SAMPLE_RATES.contains(&self.sample_rate) {
            return Err(CodecError::InvalidConfig(format!(
                "xHE-AAC sampling rate {} Hz is not allowed in DRM (use one of {:?}; resample \
                 the input first)",
                self.sample_rate, XHE_AAC_SAMPLE_RATES
            )));
        }
        if self.channels != 1 && self.channels != 2 {
            return Err(CodecError::InvalidConfig(format!(
                "xHE-AAC in DRM carries 1 or 2 channels, not {}",
                self.channels
            )));
        }
        if self.super_frame_bytes > XHE_AAC_MAX_SUPER_FRAME_BYTES {
            return Err(CodecError::InvalidConfig(format!(
                "{} bytes per super frame exceed the xHE-AAC maximum of 163 920 bit/s \
                 ({XHE_AAC_MAX_SUPER_FRAME_BYTES} bytes)",
                self.super_frame_bytes
            )));
        }
        if self.independent_interval == Some(0) {
            return Err(CodecError::InvalidConfig(
                "independent frame interval must be at least 1".into(),
            ));
        }
        self.sbr_ratio()
    }
}

// ---------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------

/// One DRM xHE-AAC access unit (a `UsacFrame()` in DRM element order), ready for the
/// xHE-AAC audio super frame builder, which appends the audio frame CRC (§5.3.1.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XheAccessUnit {
    /// The USAC access unit (whole bytes, without the DRM CRC-16).
    pub data: Vec<u8>,
    /// The 4-bit bit reservoir level of this frame for the super frame header
    /// (§5.3.1.1: the level after the first frame that starts in the super frame).
    pub bit_reservoir_level: u8,
    /// The encoder's bit reservoir fill in bits after this frame (`R_max − D`).
    pub reservoir_bits: u32,
    /// `usacIndependencyFlag` of the frame: a decoder can start here.
    pub independent: bool,
    /// Padding added to meet the channel rate (fill element and alignment) in bits.
    pub fill_bits: u32,
}

/// Counters of an [`XheAacEncoder`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XheEncoderStats {
    /// Access units produced.
    pub frames: u64,
    /// Of which with `usacIndependencyFlag = 1`.
    pub independent_frames: u64,
    /// Bytes of all access units.
    pub bytes: u64,
    /// Padding bytes (fill elements) added for the channel rate.
    pub fill_bytes: u64,
    /// Frames after which the lead exceeded the bit reservoir (should stay 0).
    pub overruns: u64,
    /// Non-fatal warnings returned by libxaac (e.g. an all-zero spectrum for silence).
    pub warnings: u64,
    /// Smallest and largest access unit in bytes.
    pub min_frame_bytes: usize,
    /// See `min_frame_bytes`.
    pub max_frame_bytes: usize,
    /// Largest lead over the channel observed, in bits (≤ the reservoir size).
    pub max_lead_bits: u64,
}

// ---------------------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------------------

/// DRM xHE-AAC encoder: libxaac (patched, see `decdrm-xaac-sys`) plus the DRM access-unit
/// conversion and rate control described in the [module docs](self).
pub struct XheAacEncoder {
    config: XheAacConfig,
    ratio: XheSbrRatio,
    channels: usize,
    frame_len: usize,
    // --- libxaac instance ---
    input_cfg: Box<ffi::ixheaace_input_config>,
    output_cfg: Box<ffi::ixheaace_output_config>,
    /// Backing store of `input_cfg.pv_drc_cfg` (u64 for 8-byte alignment).
    _drc_cfg: Box<[u64]>,
    api: NonNull<c_void>,
    in_buf: NonNull<i16>,
    out_buf: NonNull<u8>,
    out_capacity: usize,
    num_preroll: u64,
    core_bitrate: u32,
    core_bandwidth: u32,
    // --- DRM side ---
    usac_config: Vec<u8>,
    static_config: Vec<u8>,
    ledger: Ledger,
    independent_interval: u64,
    last_independent: u64,
    au_min_bytes: usize,
    frames_done: u64,
    pending: Vec<f32>,
    pcm16: Vec<i16>,
    stats: XheEncoderStats,
}

// SAFETY: the libxaac instance and every buffer it uses are exclusively owned by this value
// (allocated in `new`, freed in `drop`); libxaac has no thread-affine or shared mutable
// global state (its global tables are only read), so moving the encoder to another thread
// is sound. It is deliberately not `Sync` (libxaac does no locking).
unsafe impl Send for XheAacEncoder {}

impl Drop for XheAacEncoder {
    fn drop(&mut self) {
        // SAFETY: `output_cfg` describes the allocations of a successful `ixheaace_create`
        // and is freed exactly once here.
        unsafe { ffi::ixheaace_delete((&raw mut *self.output_cfg).cast()) };
    }
}

/// What a configuration gives the encoder, worked out without libxaac
/// ([`XheAacConfig::budget`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct XheBudget {
    /// The SBR ratio used.
    pub sbr_ratio: XheSbrRatio,
    /// Samples per channel per access unit.
    pub frame_len: usize,
    /// Channel capacity for access units in bit/s: the stream rate minus super-frame
    /// headers, frame CRCs and directory entries ([`XheAacEncoder::net_bitrate`]).
    pub net_bitrate: f64,
    /// The bit rate libxaac is asked for (a little below `net_bitrate`).
    pub core_bitrate: u32,
    /// Smallest access unit in bytes (15-frames-per-super-frame rule).
    pub min_frame_bytes: usize,
}

/// The rate parameters behind [`XheBudget`].
#[derive(Debug, Clone, Copy)]
struct RatePlan {
    ratio: XheSbrRatio,
    frame_len: usize,
    /// Channel capacity per access unit, in bits·fs (see the module docs).
    cap_num: i64,
    /// Bit rate for libxaac.
    core_rate: i64,
    au_min_bytes: usize,
}

impl XheAacConfig {
    /// Checks everything [`XheAacEncoder::new`] checks before it hands the configuration
    /// to libxaac — sampling rate, channels, super frame size, the SBR ratio
    /// ([`Self::sbr_ratio`]) and the channel budget of a frame (at least libxaac's minimum
    /// rate, room for a bit reservoir below the 6144-bit maximum, at most 15 frames per
    /// super frame) — and returns what the configuration gives the encoder. Cheap: no
    /// encoder is created, so libxaac may still reject a configuration that passes.
    pub fn budget(&self) -> Result<XheBudget, CodecError> {
        let p = self.rate_plan()?;
        Ok(XheBudget {
            sbr_ratio: p.ratio,
            frame_len: p.frame_len,
            net_bitrate: p.cap_num as f64 / p.frame_len as f64,
            core_bitrate: p.core_rate as u32,
            min_frame_bytes: p.au_min_bytes,
        })
    }

    /// The smallest [`super_frame_bytes`](Self::super_frame_bytes) for which
    /// [`Self::budget`] succeeds with this sampling rate, channel count and SBR mode, or
    /// `None` if no size does (e.g. an SBR ratio the rate does not support).
    pub fn min_super_frame_bytes(&self) -> Option<usize> {
        let mut probe = self.clone();
        (1..=XHE_AAC_MAX_SUPER_FRAME_BYTES).find(|&l| {
            probe.super_frame_bytes = l;
            probe.rate_plan().is_ok()
        })
    }

    fn rate_plan(&self) -> Result<RatePlan, CodecError> {
        let ratio = self.validate()?;
        let channels = usize::from(self.channels);
        let frame_len = ratio.frame_len();
        let fs = i64::from(self.sample_rate);
        let l = self.super_frame_bytes as i64;
        // Channel capacity per access unit, in bits·fs (see the module docs).
        let cap_num = 20 * (l - 2) * frame_len as i64 - 32 * fs;
        let avg_bits = cap_num as f64 / fs as f64;
        // libxaac runs a little below the channel rate: its own bit reservoir then keeps it
        // inside ours (both start full and have the same size); the difference is filled.
        let core_bits = avg_bits * 0.985 - 24.0;
        let core_rate = (core_bits * fs as f64 / frame_len as f64).floor() as i64;
        // libxaac's own ceiling: 6 bit per core sample and channel.
        let core_max = 6 * i64::from(ratio.core_rate(self.sample_rate)) * channels as i64;
        let core_rate = core_rate.min(core_max);
        if core_rate < LIBXAAC_MIN_BITRATE {
            return Err(CodecError::InvalidConfig(format!(
                "{} bytes per super frame leave {avg_bits:.0} bits per {frame_len}-sample \
                 xHE-AAC frame ({:.0} bit/s net), too little for the encoder",
                self.super_frame_bytes,
                avg_bits * fs as f64 / frame_len as f64
            )));
        }
        // No more than 15 frame starts per super frame: frames (access unit + CRC) of at
        // least ⌈L/15⌉ bytes guarantee that, even with a delayed border (§5.3.1.3).
        let au_min_bytes = (self
            .super_frame_bytes
            .div_ceil(XHE_AAC_MAX_FRAMES_PER_SUPER_FRAME))
        .saturating_sub(2)
        .max(1);
        // Every access unit may be up to 6144 bits per channel (§5.3.1.3); a bit reservoir
        // needs room above the average frame.
        if avg_bits > (MAX_CHANNEL_BITS - MIN_RESERVOIR_BITS) as f64 * channels as f64 {
            return Err(CodecError::InvalidConfig(format!(
                "{} bytes per super frame give {avg_bits:.0}-bit {frame_len}-sample frames at \
                 {} Hz, too close to the xHE-AAC maximum of 6144 bits per channel (ES 201 980 \
                 §5.3.1.3); use a higher sampling rate or a lower bit rate",
                self.super_frame_bytes, self.sample_rate
            )));
        }
        if (au_min_bytes * 8) as f64 >= avg_bits {
            return Err(CodecError::InvalidConfig(format!(
                "with {frame_len}-sample frames at {} Hz a 400 ms super frame holds {:.2} \
                 frames; the 15-frame limit of ES 201 980 §5.3.1 leaves no room for a bit \
                 reservoir (use SBR or a lower sampling rate)",
                self.sample_rate,
                0.4 * fs as f64 / frame_len as f64
            )));
        }
        Ok(RatePlan {
            ratio,
            frame_len,
            cap_num,
            core_rate,
            au_min_bytes,
        })
    }
}

impl XheAacEncoder {
    /// Creates and initialises an encoder.
    pub fn new(config: XheAacConfig) -> Result<Self, CodecError> {
        let RatePlan {
            ratio,
            frame_len,
            cap_num,
            core_rate,
            au_min_bytes,
        } = config.rate_plan()?;
        let channels = usize::from(config.channels);
        let fs = i64::from(config.sample_rate);

        // --- libxaac configuration (values as libxaac's test bench sets them) ---
        // SAFETY: pure function.
        let drc_size = unsafe { ffi::decdrm_xaac_drc_config_size() };
        let mut drc_cfg = vec![0u64; drc_size.div_ceil(8)].into_boxed_slice();
        let mut inp = Box::new(ffi::ixheaace_input_config::zeroed());
        inp.ui_pcm_wd_sz = 16;
        inp.aot = ffi::AOT_USAC;
        inp.usac_en = 1;
        inp.codec_mode = match config.coding_mode {
            XheCodingMode::FrequencyDomain => ffi::USAC_ONLY_FD,
            XheCodingMode::Switched => ffi::USAC_SWITCHED,
        };
        inp.ccfl_idx = i32::from(ratio.core_sbr_frame_length_index());
        inp.i_channels = channels as i32;
        inp.i_samp_freq = config.sample_rate;
        inp.i_bitrate = core_rate as i32;
        inp.i_use_es = 1;
        inp.i_use_adts = 0;
        inp.i_use_mps = 0;
        inp.i_mps_tree_config = -1;
        // eSBR auto-selection off: the SBR ratio is set explicitly.
        inp.esbr_flag = 0;
        inp.pv_drc_cfg = drc_cfg.as_mut_ptr().cast();
        inp.use_drc_element = 0;
        inp.aac_config.bitrate = core_rate as i32;
        // Core bandwidth (DecDRM patch of libxaac, see decdrm-xaac-sys): keep at least
        // BITS_PER_LINE bits per coded spectral line and channel; with SBR libxaac further
        // limits it to the SBR crossover.
        let core_nyquist = f64::from(ratio.core_rate(config.sample_rate)) / 2.0;
        let bandwidth =
            (core_rate as f64 / channels as f64 / (2.0 * BITS_PER_LINE)).min(core_nyquist);
        inp.aac_config.bandwidth = bandwidth.max(1_000.0) as i32;
        inp.aac_config.inv_quant = 2;
        inp.aac_config.use_tns = i32::from(config.tns);
        inp.aac_config.noise_filling = i32::from(config.noise_filling);
        inp.aac_config.bitreservoir_size = 768;
        // Pre-roll only at the start; independent frames are requested explicitly.
        inp.random_access_interval = -1;
        inp.method_def = ffi::METHOD_DEFINITION_PROGRAM_LOUDNESS;
        inp.measurement_system = ffi::MEASUREMENT_SYSTEM_BS_1770_3;
        inp.measured_loudness = -31.0;
        inp.sample_peak_level = -31.0;
        // Fresh input for every call, also for the withheld start-up frames.
        inp.use_delay_adjustment = 1;
        let mut out = Box::new(ffi::ixheaace_output_config::zeroed());
        out.malloc_xheaace = Some(ffi::decdrm_xaac_malloc);
        out.free_xheaace = Some(ffi::decdrm_xaac_free);

        // SAFETY: both configuration structs and the DRC buffer are valid, heap-allocated
        // and outlive the encoder; the allocator callbacks match each other.
        let err = unsafe { ffi::ixheaace_create((&raw mut *inp).cast(), (&raw mut *out).cast()) };
        if err != ffi::IA_NO_ERROR {
            // A fatal error has freed everything already; a non-fatal one may have skipped
            // the initialisation after allocating: free explicitly (a no-op if nothing is
            // left).
            // SAFETY: `out` holds the allocation records of this create call.
            unsafe { ffi::ixheaace_delete((&raw mut *out).cast()) };
            return Err(CodecError::InvalidConfig(format!(
                "libxaac rejected the configuration ({:#010x}: {})",
                err as u32,
                ffi::ia_error_name(err)
            )));
        }
        let api = NonNull::new(out.pv_ia_process_api_obj);
        let in_buf = NonNull::new(
            out.mem_info_table[ffi::IA_MEMTYPE_INPUT]
                .mem_ptr
                .cast::<i16>(),
        );
        let out_buf = NonNull::new(
            out.mem_info_table[ffi::IA_MEMTYPE_OUTPUT]
                .mem_ptr
                .cast::<u8>(),
        );
        let (Some(api), Some(in_buf), Some(out_buf)) = (api, in_buf, out_buf) else {
            // SAFETY: as above.
            unsafe { ffi::ixheaace_delete((&raw mut *out).cast()) };
            return Err(CodecError::InvalidConfig(
                "libxaac returned no encoder instance or buffers".into(),
            ));
        };
        let out_capacity = out.mem_info_table[ffi::IA_MEMTYPE_OUTPUT].ui_size as usize;
        let in_capacity = out.mem_info_table[ffi::IA_MEMTYPE_INPUT].ui_size as usize;
        let asc_len = (out.i_out_bytes.max(0) as usize).min(out_capacity);
        // SAFETY: the output buffer holds `out_capacity` bytes; create wrote the
        // AudioSpecificConfig (`i_out_bytes` bytes) at its start.
        let usac_config = unsafe { std::slice::from_raw_parts(out_buf.as_ptr(), asc_len) }.to_vec();
        // SAFETY: valid instance.
        let num_preroll = unsafe { ffi::decdrm_ixheaace_num_preroll_frames(api.as_ptr()) };
        // SAFETY: valid instance.
        let core_bandwidth = unsafe { ffi::decdrm_ixheaace_core_bandwidth(api.as_ptr()) };
        let core_bitrate = inp.i_bitrate.max(0) as u32;

        let fs_u = config.sample_rate;
        let frames_per_sf = 2 * i64::from(fs_u) / (5 * frame_len as i64); // ⌊0.4·fs/F⌋
        let independent_interval = u64::from(
            config
                .independent_interval
                .unwrap_or(frames_per_sf.max(1) as u32),
        );
        let mut enc = XheAacEncoder {
            ratio,
            channels,
            frame_len,
            input_cfg: inp,
            output_cfg: out,
            _drc_cfg: drc_cfg,
            api,
            in_buf,
            out_buf,
            out_capacity,
            num_preroll: num_preroll.max(0) as u64,
            core_bitrate,
            core_bandwidth: core_bandwidth.max(0) as u32,
            usac_config,
            static_config: Vec::new(),
            ledger: Ledger::new(fs, cap_num, MAX_CHANNEL_BITS * channels as i64),
            independent_interval,
            last_independent: 0,
            au_min_bytes,
            frames_done: 0,
            pending: Vec::new(),
            pcm16: vec![0; frame_len * channels],
            stats: XheEncoderStats {
                min_frame_bytes: usize::MAX,
                ..XheEncoderStats::default()
            },
            config,
        };

        // --- checks of what libxaac made of the configuration ---
        let inp = &enc.input_cfg;
        if inp.ccfl_idx != i32::from(ratio.core_sbr_frame_length_index())
            || inp.i_channels != channels as i32
            || inp.i_samp_freq != fs_u
            || inp.use_drc_element != 0
            || inp.i_use_mps != 0
        {
            return Err(CodecError::InvalidConfig(format!(
                "libxaac changed the configuration (ccfl_idx {}, {} channels, {} Hz, DRC {}, \
                 MPS {})",
                inp.ccfl_idx, inp.i_channels, inp.i_samp_freq, inp.use_drc_element, inp.i_use_mps
            )));
        }
        if i64::from(inp.i_bitrate) > core_rate {
            return Err(CodecError::InvalidConfig(format!(
                "libxaac raised the core bit rate to {} bit/s (channel allows {core_rate})",
                inp.i_bitrate
            )));
        }
        let input_bytes = enc.output_cfg.input_size.max(0) as usize;
        if input_bytes != frame_len * channels * 2 || in_capacity < input_bytes {
            return Err(CodecError::InvalidConfig(format!(
                "libxaac expects {input_bytes} input bytes per frame, DecDRM computed {}",
                frame_len * channels * 2
            )));
        }
        let usac = parse_audio_specific_config(&enc.usac_config)?;
        if usac.sampling_frequency != fs_u
            || usac.core_sbr_frame_length_index != ratio.core_sbr_frame_length_index()
            || usize::from(usac.channel_configuration) != channels
        {
            return Err(CodecError::Repack(format!(
                "unexpected UsacConfig from libxaac: {} Hz, coreSbrFrameLengthIndex {}, \
                 channel configuration {}",
                usac.sampling_frequency,
                usac.core_sbr_frame_length_index,
                usac.channel_configuration
            )));
        }
        enc.static_config = drm_static_config(&usac)?;
        Ok(enc)
    }

    /// The configuration.
    pub fn config(&self) -> &XheAacConfig {
        &self.config
    }

    /// Input sampling rate (= decoder output rate = SDC "audio sampling rate").
    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    /// Input channels.
    pub fn channels(&self) -> u16 {
        self.config.channels
    }

    /// The SBR ratio in use.
    pub fn sbr_ratio(&self) -> XheSbrRatio {
        self.ratio
    }

    /// Core coder sampling rate.
    pub fn core_sample_rate(&self) -> u32 {
        self.ratio.core_rate(self.config.sample_rate)
    }

    /// Input samples per channel per access unit (1024, 2048 or 4096).
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// Average number of access units per 400 ms audio super frame.
    pub fn frames_per_super_frame(&self) -> f64 {
        0.4 * f64::from(self.config.sample_rate) / self.frame_len as f64
    }

    /// Channel capacity for access units in bit/s: the stream rate minus super-frame
    /// headers, frame CRCs and directory entries.
    pub fn net_bitrate(&self) -> f64 {
        self.ledger.avg_bits() * f64::from(self.config.sample_rate) / self.frame_len as f64
    }

    /// The bit rate libxaac encodes at (a little below [`net_bitrate`](Self::net_bitrate)).
    pub fn core_bitrate(&self) -> u32 {
        self.core_bitrate
    }

    /// Audio bandwidth of the core coder in Hz (SBR, if used, reconstructs the band
    /// above it).
    pub fn core_bandwidth(&self) -> u32 {
        self.core_bandwidth
    }

    /// Size of the bit reservoir in bits (`6144·channels − average frame`).
    pub fn max_reservoir_bits(&self) -> u32 {
        (self.ledger.r_max_num / self.ledger.fs).max(0) as u32
    }

    /// Smallest access unit the encoder emits (15-frames-per-super-frame rule).
    pub fn min_frame_bytes(&self) -> usize {
        self.au_min_bytes
    }

    /// The xHE-AAC Static Config (§5.3.2, table 4) — the SDC entity 9 codec specific
    /// config field.
    pub fn static_config(&self) -> &[u8] {
        &self.static_config
    }

    /// libxaac's MPEG-4 AudioSpecificConfig with the UsacConfig (not used by DRM; for
    /// diagnostics).
    pub fn usac_audio_specific_config(&self) -> &[u8] {
        &self.usac_config
    }

    /// The SDC entity 9 audio information for this stream: xHE-AAC, mono/stereo, the
    /// sampling rate and the Static Config. Its `to_type9_bytes()` open the receiver's
    /// decoder (`open_decoder(DrmAudioCoding::XheAac, ..)`).
    pub fn audio_info(&self) -> AudioInfo {
        AudioInfo::xhe_aac(
            self.config.sample_rate,
            self.config.channels == 2,
            self.static_config.clone(),
        )
        .expect("rate validated in new()")
    }

    /// Counters.
    pub fn stats(&self) -> XheEncoderStats {
        let mut s = self.stats;
        if s.frames == 0 {
            s.min_frame_bytes = 0;
        }
        s
    }

    /// Encodes interleaved PCM (`channels` samples per instant, nominal range ±1, any
    /// length that is a whole number of instants). Returns the access units completed by
    /// this call — one per `frame_len()` input samples per channel, starting with the
    /// first block (there is no silent start-up gap in the output stream; the codec delay
    /// shows up as delayed audio inside the frames).
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<XheAccessUnit>, CodecError> {
        if !pcm.len().is_multiple_of(self.channels) {
            return Err(CodecError::InvalidInput(format!(
                "{} samples are not a whole number of {}-channel instants",
                pcm.len(),
                self.channels
            )));
        }
        self.pending.extend_from_slice(pcm);
        let per_frame = self.frame_len * self.channels;
        let mut aus = Vec::new();
        let mut offset = 0;
        let mut result = Ok(());
        while self.pending.len() - offset >= per_frame {
            for (d, &s) in self
                .pcm16
                .iter_mut()
                .zip(&self.pending[offset..offset + per_frame])
            {
                *d = (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
            }
            offset += per_frame;
            match self.encode_frame() {
                Ok(au) => aus.push(au),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        // Consumed input is dropped even after an error (libxaac errors are fatal for the
        // stream; recreate the encoder then).
        self.pending.drain(..offset);
        result.map(|()| aus)
    }

    /// Runs libxaac on `self.pcm16` and converts its frame.
    fn encode_frame(&mut self) -> Result<XheAccessUnit, CodecError> {
        let n = self.frames_done;
        let api = self.api.as_ptr();
        if n > self.num_preroll {
            let independent = n - self.last_independent >= self.independent_interval;
            // SAFETY: valid instance; called after the start-up frames as required.
            unsafe { ffi::decdrm_ixheaace_set_next_independency(api, i32::from(independent)) };
        }
        // SAFETY: the input buffer holds at least `input_size` = pcm16.len() * 2 bytes
        // (checked in `new`); the regions do not overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.pcm16.as_ptr(),
                self.in_buf.as_ptr(),
                self.pcm16.len(),
            );
        }
        // SAFETY: valid instance and configuration structs owned by `self`.
        let err = unsafe {
            ffi::ixheaace_process(
                api,
                (&raw mut *self.input_cfg).cast(),
                (&raw mut *self.output_cfg).cast(),
            )
        };
        if ffi::ia_is_fatal(err) {
            return Err(xaac_error(err, "encoding"));
        }
        if err != ffi::IA_NO_ERROR {
            self.stats.warnings += 1;
        }
        self.frames_done += 1;
        // SAFETY: valid instance.
        let frame_bits = unsafe { ffi::decdrm_ixheaace_frame_bits(api) }.max(0) as usize;
        let out_bytes = self.output_cfg.i_out_bytes.max(0) as usize;
        let withheld = out_bytes == 0;
        let avail = if withheld {
            frame_bits.div_ceil(8)
        } else {
            out_bytes
        };
        if avail == 0 || avail > self.out_capacity {
            return Err(CodecError::Repack(format!(
                "libxaac frame of {frame_bits} bits / {out_bytes} bytes does not fit its \
                 {}-byte buffer",
                self.out_capacity
            )));
        }
        // SAFETY: the output buffer holds `out_capacity >= avail` bytes, written by the call
        // above; the slice is dropped before the next libxaac call.
        let raw = unsafe { std::slice::from_raw_parts(self.out_buf.as_ptr(), avail) };
        let frame = locate_channel_element(raw, withheld, frame_bits)?;
        if frame.independent {
            self.last_independent = n;
        }

        // DRM access unit: independency flag, channel element, fill element, alignment.
        let mut w = BitBuf::new();
        w.push_bit(u32::from(frame.independent));
        let mut r = BitReader::new(raw);
        r.skip(frame.start)?;
        copy_bits(&mut r, &mut w, frame.end - frame.start)?;
        let base_bits = w.len();
        let min_bytes = (base_bits + 1).div_ceil(8);
        let needed = self.ledger.needed_bits().div_ceil(8);
        let target = needed.max(self.au_min_bytes).max(min_bytes);
        let fill = FillElement::for_target(base_bits, target);
        fill.write(&mut w);
        let bytes = w.as_bytes().to_vec();
        debug_assert_eq!(bytes.len(), fill.total_bytes(base_bits));

        let au_bits = bytes.len() as i64 * 8;
        self.ledger.push(au_bits);
        let lead = self.ledger.lead_bits();
        let reservoir = (self.ledger.reservoir_bits()).max(0);
        if self.ledger.overrun() {
            self.stats.overruns += 1;
        }
        let level = (reservoir / (BIT_RES_LEVEL_STEP * self.channels as i64)).clamp(0, 15) as u8;
        let fill_bits = (bytes.len() * 8 - (base_bits + 1)) as u32;

        let s = &mut self.stats;
        s.frames += 1;
        s.independent_frames += u64::from(frame.independent);
        s.bytes += bytes.len() as u64;
        s.fill_bytes += u64::from(fill_bits / 8);
        s.min_frame_bytes = s.min_frame_bytes.min(bytes.len());
        s.max_frame_bytes = s.max_frame_bytes.max(bytes.len());
        s.max_lead_bits = s.max_lead_bits.max(lead.max(0) as u64);
        Ok(XheAccessUnit {
            data: bytes,
            bit_reservoir_level: level,
            reservoir_bits: reservoir as u32,
            independent: frame.independent,
            fill_bits,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Rate ledger
// ---------------------------------------------------------------------------------------

/// Exact bookkeeping of the encoder's lead over the DRM channel, in units of 1/fs bit.
#[derive(Debug, Clone)]
struct Ledger {
    /// Denominator (the sampling rate).
    fs: i64,
    /// Channel capacity per access unit, bits·fs.
    cap_num: i64,
    /// Lead `D` (Σ access-unit bits − n·avg), bits·fs.
    lead_num: i64,
    /// Reservoir size `R_max = 6144·channels − avg`, bits·fs.
    r_max_num: i64,
}

impl Ledger {
    fn new(fs: i64, cap_num: i64, max_frame_bits: i64) -> Self {
        Self {
            fs,
            cap_num,
            lead_num: 0,
            r_max_num: max_frame_bits * fs - cap_num,
        }
    }

    fn avg_bits(&self) -> f64 {
        self.cap_num as f64 / self.fs as f64
    }

    /// Bits the next access unit needs at least so that the lead stays ≥ 0.
    fn needed_bits(&self) -> usize {
        let deficit = self.cap_num - self.lead_num;
        if deficit <= 0 {
            0
        } else {
            (deficit + self.fs - 1).div_euclid(self.fs) as usize
        }
    }

    fn push(&mut self, bits: i64) {
        self.lead_num += bits * self.fs - self.cap_num;
    }

    fn lead_bits(&self) -> i64 {
        self.lead_num.div_euclid(self.fs)
    }

    /// `R_max − D` rounded down.
    fn reservoir_bits(&self) -> i64 {
        (self.r_max_num - self.lead_num).div_euclid(self.fs)
    }

    fn overrun(&self) -> bool {
        self.lead_num > self.r_max_num
    }
}

// ---------------------------------------------------------------------------------------
// Frame layout and fill element
// ---------------------------------------------------------------------------------------

/// Where the channel element of a libxaac frame lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameLayout {
    independent: bool,
    /// First bit of the channel element.
    start: usize,
    /// One past its last bit.
    end: usize,
}

/// Finds the channel element in a libxaac output frame of `frame_bits` exact bits.
///
/// * Normal frame (also the withheld start-up frames): `indep(1) · preroll present = 0 (1) ·
///   channel element`, `frame_bits` counted from the first bit.
/// * Start-up AudioPreRoll frame: `indep = 1 · present = 1 · useDefaultLength = 0 ·
///   payloadLength (8, escape 16) · payload · channel element`; there `frame_bits` counts
///   only the embedded channel element (libxaac encodes it without the two leading bits).
fn locate_channel_element(
    raw: &[u8],
    withheld: bool,
    frame_bits: usize,
) -> Result<FrameLayout, CodecError> {
    let total = raw.len() * 8;
    let mut r = BitReader::new(raw);
    let independent = r.bit()? == 1;
    let preroll_present = r.bit()? == 1;
    let (start, end) = if !preroll_present {
        (2, frame_bits)
    } else {
        if withheld || !independent {
            return Err(CodecError::Repack(
                "unexpected AudioPreRoll element in a libxaac frame".into(),
            ));
        }
        if r.bit()? != 0 {
            return Err(CodecError::Repack(
                "AudioPreRoll with default length (not configured)".into(),
            ));
        }
        let mut len = r.bits(8)? as usize;
        if len == 255 {
            len = 255 + r.bits(16)? as usize - 2;
        }
        let start = r.position() + 8 * len;
        (start, start + frame_bits)
    };
    if end <= start || end > total {
        return Err(CodecError::Repack(format!(
            "libxaac frame layout inconsistent: channel element bits {start}..{end} of {total}"
        )));
    }
    Ok(FrameLayout {
        independent,
        start,
        end,
    })
}

fn copy_bits(r: &mut BitReader<'_>, w: &mut BitBuf, mut n: usize) -> Result<(), CodecError> {
    while n >= 32 {
        w.push(u64::from(r.bits(32)?), 32);
        n -= 32;
    }
    if n > 0 {
        w.push(u64::from(r.bits(n as u32)?), n as u32);
    }
    Ok(())
}

/// The `ID_EXT_ELE_FILL` element closing a DRM access unit (ISO/IEC 23003-3
/// `UsacExtElement()`): absent (1 bit), or present with `payload` fill bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FillElement {
    payload: Option<usize>,
}

impl FillElement {
    /// Header bits for a payload of `n` bytes: present, useDefaultLength, length (8 bits,
    /// plus 16 when the escape value 255 is needed).
    fn header_bits(n: usize) -> usize {
        if n < 255 { 1 + 1 + 8 } else { 1 + 1 + 8 + 16 }
    }

    /// Total access unit bytes when the frame so far has `base_bits` bits.
    fn total_bytes(self, base_bits: usize) -> usize {
        match self.payload {
            None => (base_bits + 1).div_ceil(8),
            Some(n) => (base_bits + Self::header_bits(n) + 8 * n).div_ceil(8),
        }
    }

    /// The smallest element that makes the access unit at least `target` bytes long (the
    /// result can exceed `target` by a byte or two where no payload size fits exactly).
    fn for_target(base_bits: usize, target: usize) -> Self {
        let none = Self { payload: None };
        if none.total_bytes(base_bits) >= target {
            return none;
        }
        let bits = 8 * target;
        // Short form (payload < 255 bytes).
        let short = bits.saturating_sub(base_bits + 10) / 8;
        if short < 255 {
            return Self {
                payload: Some(short),
            };
        }
        // Long form.
        let long = (bits.saturating_sub(base_bits + 26) / 8).max(255);
        Self {
            payload: Some(long),
        }
    }

    fn write(self, w: &mut BitBuf) {
        match self.payload {
            None => w.push_bit(0),
            Some(n) => {
                w.push_bit(1); // usacExtElementPresent
                w.push_bit(0); // usacExtElementUseDefaultLength
                if n < 255 {
                    w.push(n as u64, 8);
                } else {
                    // usacExtElementPayloadLength = 255 + valueAdd − 2
                    w.push(255, 8);
                    w.push((n - 255 + 2) as u64, 16);
                }
                // usacExtElementPayloadFrag = 0: no start/stop bits.
                for _ in 0..n {
                    w.push(FILL_BYTE, 8);
                }
            }
        }
        // byte_alignment()
        while !w.len().is_multiple_of(8) {
            w.push_bit(0);
        }
    }
}

// ---------------------------------------------------------------------------------------
// UsacConfig → xHE-AAC Static Config
// ---------------------------------------------------------------------------------------

/// `escapedValue(n1, n2, n3)` (ISO/IEC 23003-3).
fn read_escaped(r: &mut BitReader<'_>, n1: u32, n2: u32, n3: u32) -> Result<u32, CodecError> {
    let mut v = r.bits(n1)?;
    if v == (1 << n1) - 1 && n2 > 0 {
        let v2 = r.bits(n2)?;
        v += v2;
        if v2 == (1 << n2) - 1 && n3 > 0 {
            v += r.bits(n3)?;
        }
    }
    Ok(v)
}

fn write_escaped(w: &mut BitBuf, value: u32, n1: u32, n2: u32, n3: u32) {
    let max1 = (1u32 << n1) - 1;
    let v1 = value.min(max1);
    w.push(u64::from(v1), n1);
    if v1 == max1 && n2 > 0 {
        let rest = value - v1;
        let max2 = (1u32 << n2) - 1;
        let v2 = rest.min(max2);
        w.push(u64::from(v2), n2);
        if v2 == max2 && n3 > 0 {
            w.push(u64::from(rest - v2), n3);
        }
    }
}

/// `SbrConfig()` with its `SbrDfltHeader()` (ISO/IEC 23003-3 table 18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SbrConfig {
    harmonic_sbr: bool,
    inter_tes: bool,
    pvc: bool,
    start_freq: u8,
    stop_freq: u8,
    /// `dflt_freq_scale`, `dflt_alter_scale`, `dflt_noise_bands`.
    extra1: Option<(u8, u8, u8)>,
    /// `dflt_limiter_bands`, `dflt_limiter_gains`, `dflt_interpol_freq`,
    /// `dflt_smoothing_mode`.
    extra2: Option<(u8, u8, u8, u8)>,
}

impl SbrConfig {
    fn read(r: &mut BitReader<'_>) -> Result<Self, CodecError> {
        let harmonic_sbr = r.bit()? == 1;
        let inter_tes = r.bit()? == 1;
        let pvc = r.bit()? == 1;
        let start_freq = r.bits(4)? as u8;
        let stop_freq = r.bits(4)? as u8;
        let has1 = r.bit()? == 1;
        let has2 = r.bit()? == 1;
        let extra1 = if has1 {
            Some((r.bits(2)? as u8, r.bits(1)? as u8, r.bits(2)? as u8))
        } else {
            None
        };
        let extra2 = if has2 {
            Some((
                r.bits(2)? as u8,
                r.bits(2)? as u8,
                r.bits(1)? as u8,
                r.bits(1)? as u8,
            ))
        } else {
            None
        };
        Ok(Self {
            harmonic_sbr,
            inter_tes,
            pvc,
            start_freq,
            stop_freq,
            extra1,
            extra2,
        })
    }

    fn write(&self, w: &mut BitBuf) {
        w.push_bit(u32::from(self.harmonic_sbr));
        w.push_bit(u32::from(self.inter_tes));
        w.push_bit(u32::from(self.pvc));
        w.push(u64::from(self.start_freq), 4);
        w.push(u64::from(self.stop_freq), 4);
        w.push_bit(u32::from(self.extra1.is_some()));
        w.push_bit(u32::from(self.extra2.is_some()));
        if let Some((a, b, c)) = self.extra1 {
            w.push(u64::from(a), 2);
            w.push(u64::from(b), 1);
            w.push(u64::from(c), 2);
        }
        if let Some((a, b, c, d)) = self.extra2 {
            w.push(u64::from(a), 2);
            w.push(u64::from(b), 2);
            w.push(u64::from(c), 1);
            w.push(u64::from(d), 1);
        }
    }
}

/// One element of `UsacDecoderConfig()`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UsacElement {
    Sce {
        tw_mdct: bool,
        noise_filling: bool,
        sbr: Option<SbrConfig>,
    },
    Cpe {
        tw_mdct: bool,
        noise_filling: bool,
        sbr: Option<SbrConfig>,
        stereo_config_index: u8,
    },
    Lfe,
    Ext {
        ext_type: u32,
        config_len: u32,
        default_length: Option<u32>,
        payload_frag: bool,
    },
}

/// The parts of an AudioSpecificConfig with UsacConfig that DRM needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UsacConfigInfo {
    sampling_frequency: u32,
    core_sbr_frame_length_index: u8,
    channel_configuration: u8,
    elements: Vec<UsacElement>,
}

/// Parses an MPEG-4 AudioSpecificConfig (AOT 42) with its UsacConfig (ISO/IEC 23003-3
/// §5.2, tables 13–20). Config extensions are skipped; MPS212 is rejected.
fn parse_audio_specific_config(asc: &[u8]) -> Result<UsacConfigInfo, CodecError> {
    let mut r = BitReader::new(asc);
    let mut aot = r.bits(5)?;
    if aot == 31 {
        aot = 32 + r.bits(6)?;
    }
    if aot != 42 {
        return Err(CodecError::Repack(format!(
            "AudioSpecificConfig has object type {aot}, not USAC (42)"
        )));
    }
    if r.bits(4)? == 0xF {
        r.skip(24)?;
    }
    let _asc_channel_configuration = r.bits(4)?;
    // UsacConfig()
    let sf_index = r.bits(5)? as usize;
    let sampling_frequency = if sf_index == 0x1F {
        r.bits(24)?
    } else {
        USAC_SAMPLING_FREQUENCIES[sf_index]
    };
    let core_sbr_frame_length_index = r.bits(3)? as u8;
    let sbr_present = matches!(core_sbr_frame_length_index, 2..=4);
    let channel_configuration = r.bits(5)? as u8;
    if channel_configuration == 0 {
        return Err(CodecError::Repack(
            "UsacConfig with explicit channel layout is not supported".into(),
        ));
    }
    // UsacDecoderConfig()
    let num_elements = read_escaped(&mut r, 4, 8, 16)? as usize + 1;
    let mut elements = Vec::with_capacity(num_elements);
    for _ in 0..num_elements {
        let element = match r.bits(2)? {
            0 => {
                let tw_mdct = r.bit()? == 1;
                let noise_filling = r.bit()? == 1;
                let sbr = if sbr_present {
                    Some(SbrConfig::read(&mut r)?)
                } else {
                    None
                };
                UsacElement::Sce {
                    tw_mdct,
                    noise_filling,
                    sbr,
                }
            }
            1 => {
                let tw_mdct = r.bit()? == 1;
                let noise_filling = r.bit()? == 1;
                let (sbr, stereo_config_index) = if sbr_present {
                    (Some(SbrConfig::read(&mut r)?), r.bits(2)? as u8)
                } else {
                    (None, 0)
                };
                if stereo_config_index > 0 {
                    return Err(CodecError::Repack(
                        "MPS212 (stereoConfigIndex > 0) is not supported".into(),
                    ));
                }
                UsacElement::Cpe {
                    tw_mdct,
                    noise_filling,
                    sbr,
                    stereo_config_index,
                }
            }
            2 => UsacElement::Lfe,
            _ => {
                let ext_type = read_escaped(&mut r, 4, 8, 16)?;
                let config_len = read_escaped(&mut r, 4, 8, 16)?;
                let default_length = if r.bit()? == 1 {
                    Some(read_escaped(&mut r, 8, 16, 0)? + 1)
                } else {
                    None
                };
                let payload_frag = r.bit()? == 1;
                r.skip(8 * config_len as usize)?;
                UsacElement::Ext {
                    ext_type,
                    config_len,
                    default_length,
                    payload_frag,
                }
            }
        };
        elements.push(element);
    }
    Ok(UsacConfigInfo {
        sampling_frequency,
        core_sbr_frame_length_index,
        channel_configuration,
        elements,
    })
}

/// Builds the DRM xHE-AAC Static Config (ES 201 980 §5.3.2, tables 4–7) for frames in
/// DecDRM's layout: the channel element of `usac` as element 0, one `ID_EXT_ELE_FILL`
/// extension element, no config extension; zero-padded to whole bytes (§6.4.3.10).
///
/// `usac` must contain exactly one channel element (SCE for mono, CPE for stereo) and
/// otherwise only the AudioPreRoll extension element that the access-unit conversion
/// removes.
fn drm_static_config(usac: &UsacConfigInfo) -> Result<Vec<u8>, CodecError> {
    let index = usac.core_sbr_frame_length_index;
    if XheSbrRatio::from_index(index).is_none() {
        return Err(CodecError::Repack(format!(
            "coreSbrFrameLengthIndex {index} cannot be signalled in DRM (1..=4)"
        )));
    }
    let mut channel_element = None;
    for e in &usac.elements {
        match e {
            UsacElement::Sce { .. } | UsacElement::Cpe { .. } if channel_element.is_none() => {
                channel_element = Some(e);
            }
            UsacElement::Ext {
                ext_type: ID_EXT_ELE_AUDIOPREROLL,
                config_len: 0,
                ..
            } if channel_element.is_none() => {}
            other => {
                return Err(CodecError::Repack(format!(
                    "USAC element {other:?} cannot be carried in DRM by DecDRM"
                )));
            }
        }
    }
    let mut w = BitBuf::new();
    w.push(u64::from(index - 1), 2); // coreSbrFrameLengthIndexDrm
    match channel_element {
        Some(UsacElement::Sce {
            tw_mdct: false,
            noise_filling,
            sbr,
        }) if usac.channel_configuration == 1 => {
            w.push_bit(u32::from(*noise_filling));
            if let Some(sbr) = sbr {
                sbr.write(&mut w);
            }
        }
        Some(UsacElement::Cpe {
            tw_mdct: false,
            noise_filling,
            sbr,
            stereo_config_index: 0,
        }) if usac.channel_configuration == 2 => {
            w.push_bit(u32::from(*noise_filling));
            if let Some(sbr) = sbr {
                sbr.write(&mut w);
                w.push(0, 2); // stereoConfigIndex
            }
        }
        other => {
            return Err(CodecError::Repack(format!(
                "channel element {other:?} does not match channel configuration {}",
                usac.channel_configuration
            )));
        }
    }
    write_escaped(&mut w, 1, 2, 4, 8); // numExtElements
    // UsacExtElementConfig() of the fill element.
    write_escaped(&mut w, ID_EXT_ELE_FILL, 4, 8, 16); // usacExtElementType
    write_escaped(&mut w, 0, 4, 8, 16); // usacExtElementConfigLength
    w.push_bit(0); // usacExtElementDefaultLengthPresent
    w.push_bit(0); // usacExtElementPayloadFrag
    w.push_bit(0); // usacConfigExtensionPresent
    Ok(w.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_values_roundtrip() {
        for (n1, n2, n3) in [(4, 8, 16), (2, 4, 8), (8, 16, 0), (5, 8, 16)] {
            let max =
                (1u32 << n1) - 1 + (1u32 << n2) - 1 + if n3 > 0 { (1u32 << n3) - 1 } else { 0 };
            for v in [
                0,
                1,
                (1 << n1) - 2,
                (1 << n1) - 1,
                (1 << n1),
                max.min(70_000),
            ] {
                if v > max {
                    continue;
                }
                let mut w = BitBuf::new();
                write_escaped(&mut w, v, n1, n2, n3);
                let mut r = BitReader::new(w.as_bytes());
                assert_eq!(
                    read_escaped(&mut r, n1, n2, n3).unwrap(),
                    v,
                    "{v} ({n1},{n2},{n3})"
                );
                assert_eq!(r.position(), w.len());
            }
        }
    }

    #[test]
    fn fill_element_hits_every_reachable_size() {
        for base in [1usize, 7, 8, 9, 100, 1234] {
            let min = FillElement { payload: None }.total_bytes(base);
            for target in 0..min + 600 {
                let f = FillElement::for_target(base, target);
                let total = f.total_bytes(base);
                assert!(
                    total >= target.max(min),
                    "base {base} target {target}: {total}"
                );
                // At most 3 bytes above the target (1 around the short form's start, 3 at
                // the switch to the long form).
                assert!(
                    total <= target.max(min) + 3,
                    "base {base} target {target}: {total}"
                );
                let mut w = BitBuf::zeros(base);
                f.write(&mut w);
                assert_eq!(w.as_bytes().len(), total);
                // Parse it back as a UsacExtElement.
                let mut r = BitReader::new(w.as_bytes());
                r.skip(base).unwrap();
                if r.bit().unwrap() == 1 {
                    assert_eq!(r.bit().unwrap(), 0);
                    let mut len = r.bits(8).unwrap() as usize;
                    if len == 255 {
                        len = 255 + r.bits(16).unwrap() as usize - 2;
                    }
                    assert_eq!(Some(len), f.payload);
                    for _ in 0..len {
                        assert_eq!(r.bits(8).unwrap(), 0xA5);
                    }
                } else {
                    assert_eq!(f.payload, None);
                }
                assert!(r.remaining() < 8, "only alignment may follow");
            }
        }
    }

    #[test]
    fn ledger_tracks_the_channel_exactly() {
        // 16 kbit/s, 2048-sample frames at 24 kHz: 798 payload bytes per 400 ms for 4.6875
        // frames, minus 4 bytes per frame: 166.24 bytes = 1329.92 bits per frame.
        let fs = 24_000;
        let cap = 20 * 798 * 2048 - 32 * fs;
        let mut l = Ledger::new(fs, cap, 2 * 6144);
        assert!((l.avg_bits() - 1329.92).abs() < 1e-9);
        assert_eq!(l.needed_bits(), 1330);
        let mut total = 0i64;
        for n in 1..=1000i64 {
            let bytes = (l.needed_bits().div_ceil(8)) as i64;
            l.push(bytes * 8);
            total += bytes * 8;
            assert!(
                l.lead_num >= 0 && l.lead_bits() < 8,
                "frame {n}: lead {}",
                l.lead_bits()
            );
            // Cumulative bits never fall behind the channel and stay within a byte of it.
            assert!(total * fs >= n * cap && (total - 8) * fs < n * cap);
        }
        assert_eq!(
            l.reservoir_bits(),
            (2 * 6144 * fs - cap - l.lead_num).div_euclid(fs)
        );
    }

    fn sbr(start: u8) -> SbrConfig {
        SbrConfig {
            harmonic_sbr: false,
            inter_tes: false,
            pvc: false,
            start_freq: start,
            stop_freq: 4,
            extra1: None,
            extra2: None,
        }
    }

    #[test]
    fn static_config_layout() {
        // Stereo, 2:1 SBR, noise filling, as libxaac configures it.
        let usac = UsacConfigInfo {
            sampling_frequency: 24_000,
            core_sbr_frame_length_index: 3,
            channel_configuration: 2,
            elements: vec![
                UsacElement::Ext {
                    ext_type: ID_EXT_ELE_AUDIOPREROLL,
                    config_len: 0,
                    default_length: None,
                    payload_frag: false,
                },
                UsacElement::Cpe {
                    tw_mdct: false,
                    noise_filling: true,
                    sbr: Some(sbr(0)),
                    stereo_config_index: 0,
                },
            ],
        };
        let cfg = drm_static_config(&usac).unwrap();
        // 2 + 1 + 3 + 4 + 4 + 2 + 2 + 2 + 4 + 4 + 1 + 1 + 1 = 31 bits -> 4 bytes:
        // 10 1 000 0000 | 0100 00 00 | 01 0000 00 | 00 0 0 0 (+1 pad)
        assert_eq!(
            cfg,
            vec![0b1010_0000, 0b0001_0000, 0b0001_0000, 0b0000_0000]
        );
        let mut r = BitReader::new(&cfg);
        assert_eq!(r.bits(2).unwrap(), 2, "coreSbrFrameLengthIndexDrm");
        assert_eq!(r.bit().unwrap(), 1, "noiseFilling");
        assert_eq!(SbrConfig::read(&mut r).unwrap(), sbr(0));
        assert_eq!(r.bits(2).unwrap(), 0, "stereoConfigIndex");
        assert_eq!(read_escaped(&mut r, 2, 4, 8).unwrap(), 1, "numExtElements");
        assert_eq!(read_escaped(&mut r, 4, 8, 16).unwrap(), ID_EXT_ELE_FILL);

        // Mono without SBR: 2 + 1 + 2 + 10 + 1 = 16 bits.
        let mono = UsacConfigInfo {
            sampling_frequency: 12_000,
            core_sbr_frame_length_index: 1,
            channel_configuration: 1,
            elements: vec![
                UsacElement::Ext {
                    ext_type: ID_EXT_ELE_AUDIOPREROLL,
                    config_len: 0,
                    default_length: None,
                    payload_frag: false,
                },
                UsacElement::Sce {
                    tw_mdct: false,
                    noise_filling: false,
                    sbr: None,
                },
            ],
        };
        assert_eq!(
            drm_static_config(&mono).unwrap(),
            vec![0b0000_1000, 0b0000_0000]
        );

        // Rejected: 768-sample core without SBR, time-warped MDCT, a DRC element.
        let mut bad = mono.clone();
        bad.core_sbr_frame_length_index = 0;
        assert!(drm_static_config(&bad).is_err());
        let mut bad = mono.clone();
        bad.elements[1] = UsacElement::Sce {
            tw_mdct: true,
            noise_filling: false,
            sbr: None,
        };
        assert!(drm_static_config(&bad).is_err());
        let mut bad = mono;
        bad.elements.push(UsacElement::Ext {
            ext_type: 4,
            config_len: 3,
            default_length: None,
            payload_frag: false,
        });
        assert!(drm_static_config(&bad).is_err());
    }

    #[test]
    fn frame_layouts() {
        // Normal frame: indep 1, preroll 0, then 11 bits of channel element.
        let mut w = BitBuf::new();
        w.push(0b10, 2);
        w.push(0b101_1001_1101, 11);
        let raw = w.as_bytes().to_vec();
        let f = locate_channel_element(&raw, false, 13).unwrap();
        assert_eq!((f.independent, f.start, f.end), (true, 2, 13));
        // AudioPreRoll frame with a 3-byte payload, then 9 channel element bits.
        let mut w = BitBuf::new();
        w.push(0b110, 3);
        w.push(3, 8);
        w.push(0xABCDEF, 24);
        w.push(0x1FF, 9);
        let raw = w.as_bytes().to_vec();
        let f = locate_channel_element(&raw, false, 9).unwrap();
        assert_eq!((f.start, f.end), (35, 44));
        // A pre-roll element in a withheld frame or a frame claiming too many bits.
        assert!(locate_channel_element(&raw, true, 9).is_err());
        assert!(locate_channel_element(&raw, false, 100).is_err());
    }

    /// FDK-AAC decoder for a raw MPEG-4 stream configured with an AudioSpecificConfig or
    /// for DRM configured with SDC type-9 bytes.
    struct FdkRef {
        h: *mut decdrm_fdk_sys::AAC_DECODER_INSTANCE,
        out: Vec<i16>,
    }

    impl FdkRef {
        fn new(transport: decdrm_fdk_sys::TRANSPORT_TYPE, conf: &[u8]) -> Self {
            use decdrm_fdk_sys as fdk;
            // SAFETY: plain constructor; the configuration buffer outlives the call.
            unsafe {
                let h = fdk::aacDecoder_Open(transport, 1);
                assert!(!h.is_null());
                let mut c = conf.to_vec();
                let mut p = c.as_mut_ptr();
                let len = c.len() as u32;
                assert_eq!(fdk::aacDecoder_ConfigRaw(h, &mut p, &len), 0, "FDK config");
                // No loudness normalisation: libxaac's UsacConfig carries a default loudness
                // info (not a measurement) that DecDRM drops from the DRM Static Config.
                assert_eq!(
                    fdk::aacDecoder_SetParam(h, fdk::AAC_DRC_REFERENCE_LEVEL, -1),
                    0
                );
                Self {
                    h,
                    out: vec![0; 4096 * 8],
                }
            }
        }

        /// Decodes one access unit; returns the PCM (or None if FDK produced no output).
        fn decode(&mut self, au: &[u8]) -> Option<Vec<i16>> {
            use decdrm_fdk_sys as fdk;
            let mut buf = au.to_vec();
            let mut p = buf.as_mut_ptr();
            let size = buf.len() as u32;
            let mut valid = size;
            // SAFETY: pointer/length of the live `buf`; output buffer of `out.len()` samples.
            unsafe {
                assert_eq!(fdk::aacDecoder_Fill(self.h, &mut p, &size, &mut valid), 0);
                let err = fdk::aacDecoder_DecodeFrame(
                    self.h,
                    self.out.as_mut_ptr(),
                    self.out.len() as i32,
                    0,
                );
                if !fdk::IS_OUTPUT_VALID(err) {
                    return None;
                }
                assert_eq!(err, 0, "FDK decode error {err:#x}");
                let si = *fdk::aacDecoder_GetStreamInfo(self.h);
                let n = (si.frameSize * si.numChannels) as usize;
                Some(self.out[..n].to_vec())
            }
        }
    }

    impl Drop for FdkRef {
        fn drop(&mut self) {
            // SAFETY: opened in new(), closed once.
            unsafe { decdrm_fdk_sys::aacDecoder_Close(self.h) }
        }
    }

    /// The DRM access units carry libxaac's channel elements unchanged: FDK decodes the
    /// DRM stream (TT_DRM, configured from SDC type 9) to exactly the PCM it decodes from
    /// libxaac's own MPEG stream (raw transport, AudioSpecificConfig, AudioPreRoll).
    fn drm_repack_is_lossless(cfg: XheAacConfig, frames: usize) -> (usize, usize) {
        let mut enc = XheAacEncoder::new(cfg).unwrap();
        let mut raw_dec = FdkRef::new(decdrm_fdk_sys::TT_MP4_RAW, enc.usac_audio_specific_config());
        let mut drm_dec = FdkRef::new(decdrm_fdk_sys::TT_DRM, &enc.audio_info().to_type9_bytes());
        let (fs, ch, n) = (
            enc.sample_rate() as f64,
            enc.channels() as usize,
            enc.frame_len(),
        );
        let (mut raw_pcm, mut drm_pcm) = (Vec::new(), Vec::new());
        let mut withheld = 0;
        for f in 0..frames {
            let pcm: Vec<f32> = (0..n * ch)
                .map(|i| {
                    let t = (f * n + i / ch) as f64 / fs;
                    let fr = if i % ch == 0 { 440.0 } else { 660.0 };
                    (0.35 * (2.0 * std::f64::consts::PI * fr * t).sin()
                        + 0.15 * (2.0 * std::f64::consts::PI * 1250.0 * t).sin())
                        as f32
                })
                .collect();
            let aus = enc.encode(&pcm).unwrap();
            assert_eq!(aus.len(), 1);
            // libxaac's own access unit of this call (empty while it withholds frames).
            let out_bytes = enc.output_cfg.i_out_bytes.max(0) as usize;
            if out_bytes == 0 {
                withheld += 1;
            } else {
                // SAFETY: libxaac wrote `out_bytes` bytes to its output buffer.
                let raw =
                    unsafe { std::slice::from_raw_parts(enc.out_buf.as_ptr(), out_bytes) }.to_vec();
                if let Some(p) = raw_dec.decode(&raw) {
                    raw_pcm.extend(p);
                }
            }
            let mut drm = aus[0].data.clone();
            drm.extend_from_slice(&[0x12, 0x34]); // the receiver passes the CRC along
            if let Some(p) = drm_dec.decode(&drm) {
                drm_pcm.extend(p);
            }
        }
        // The DRM stream also decodes the frames libxaac put into its AudioPreRoll; FDK
        // fades in the first frame after an AudioPreRoll (≈5 ms), so compare from the
        // second frame on.
        let skip = withheld * n * ch;
        assert!(drm_pcm.len() > skip + raw_pcm.len() / 2);
        let common = raw_pcm.len().min(drm_pcm.len() - skip);
        let from = n * ch;
        let diff = raw_pcm[from..common]
            .iter()
            .zip(&drm_pcm[skip + from..skip + common])
            .filter(|(a, b)| a != b)
            .count();
        (diff, common - from)
    }

    #[test]
    fn drm_repack_decodes_like_the_mpeg_stream() {
        for (rate, ch, br) in [
            (24_000, 2, 12_000),
            (24_000, 1, 12_000),
            (48_000, 2, 32_000),
        ] {
            let (diff, n) = drm_repack_is_lossless(XheAacConfig::new(rate, ch, br), 40);
            assert_eq!(
                diff, 0,
                "{rate} Hz {ch} ch {br}: {diff} of {n} samples differ"
            );
        }
    }

    #[test]
    fn config_validation() {
        let ok = XheAacConfig::new(24_000, 2, 16_000);
        assert_eq!(ok.super_frame_bytes, 800);
        assert_eq!(ok.sbr_ratio().unwrap(), XheSbrRatio::Ratio2To1);
        assert_eq!(
            XheAacConfig::new(48_000, 1, 8_000).sbr_ratio().unwrap(),
            XheSbrRatio::Ratio4To1
        );
        assert_eq!(
            XheAacConfig::new(48_000, 1, 12_000).sbr_ratio().unwrap(),
            XheSbrRatio::Ratio2To1
        );
        assert_eq!(
            XheAacConfig::new(38_400, 1, 8_000).sbr_ratio().unwrap(),
            XheSbrRatio::Ratio4To1
        );
        assert!(XheAacConfig::new(38_400, 2, 16_000).sbr_ratio().is_err());
        assert!(XheAacConfig::new(48_000, 2, 8_000).sbr_ratio().is_err());
        let mut fixed = XheAacConfig::new(38_400, 1, 16_000);
        fixed.sbr = XheSbrMode::Fixed(XheSbrRatio::Ratio2To1);
        assert!(fixed.sbr_ratio().is_err());
        assert_eq!(
            XheAacConfig::new(12_000, 1, 12_000).sbr_ratio().unwrap(),
            XheSbrRatio::None
        );
        assert!(XheAacConfig::new(44_100, 2, 16_000).validate().is_err());
        assert!(XheAacConfig::new(24_000, 3, 16_000).validate().is_err());
        let mut st41 = XheAacConfig::new(48_000, 2, 16_000);
        st41.sbr = XheSbrMode::Fixed(XheSbrRatio::Ratio4To1);
        assert!(st41.validate().is_err());
        let mut nosbr48 = XheAacConfig::new(48_000, 2, 64_000);
        nosbr48.sbr = XheSbrMode::Fixed(XheSbrRatio::None);
        assert!(nosbr48.validate().is_err());
    }

    /// `budget` agrees with the encoder, and `min_super_frame_bytes` is the boundary.
    #[test]
    fn budget_without_libxaac() {
        let cfg = XheAacConfig::new(24_000, 2, 16_000);
        let b = cfg.budget().unwrap();
        let enc = XheAacEncoder::new(cfg.clone()).unwrap();
        assert_eq!(b.sbr_ratio, enc.sbr_ratio());
        assert_eq!(b.frame_len, enc.frame_len());
        assert!((b.net_bitrate - enc.net_bitrate()).abs() < 1e-9);
        assert_eq!(b.core_bitrate, enc.core_bitrate());
        assert_eq!(b.min_frame_bytes, enc.min_frame_bytes());
        // Around 4.8 kbit/s at 24 kHz (libxaac's 4 kbit/s minimum plus the overhead).
        let min = cfg.min_super_frame_bytes().unwrap();
        assert!((230..250).contains(&min), "{min}");
        let at = |l: usize| XheAacConfig::with_super_frame_bytes(24_000, 2, l);
        assert!(at(min).budget().is_ok() && at(min - 1).budget().is_err());
        assert!(XheAacEncoder::new(at(min)).is_ok());
        // No size works for 38.4 kHz stereo.
        assert_eq!(
            XheAacConfig::new(38_400, 2, 16_000).min_super_frame_bytes(),
            None
        );
        // Stereo at 48 kHz: from 12 kbit/s.
        assert_eq!(
            XheAacConfig::new(48_000, 2, 16_000).min_super_frame_bytes(),
            Some(600)
        );
    }
}
