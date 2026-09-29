//! Build and raw FFI bindings for the vendored Fraunhofer FDK-AAC library.
//!
//! This crate compiles `third_party/fdk-aac` (tag v2.0.3) from source into a static
//! library (see `build.rs`) and exposes a *hand-written* subset of its C API: the AAC
//! decoder (`aacdecoder_lib.h`), the AAC encoder (`aacenc_lib.h`) and the shared types of
//! `FDK_audio.h`. There is no bindgen (no libclang on the build machines); instead a
//! small C++ shim reports `sizeof`/`offsetof` for every transcribed struct and the unit
//! tests below compare them with Rust's view.
//!
//! Everything here is `unsafe` and C-shaped on purpose; `decdrm-codecs` wraps it in a
//! safe API. Names follow the C headers (hence the lint allowances).
//!
//! # DecDRM modifications
//!
//! The encoder is built with a few anchored source patches (see `build.rs`) — it is a
//! "Third-Party Modified Version of the Fraunhofer FDK AAC Codec Library":
//!
//! * [`AACENC_GRANULE_LENGTH`] accepts **960** (DRM's transform length);
//! * the private parameter [`AACENC_DECDRM_DRM_SBR`] makes the SBR encoder emit
//!   DRM-syntax SBR payloads (scalable mono syntax plus the 8-bit DRM SBR CRC), still
//!   carried in an ordinary MPEG-4 fill element of a raw (`TT_MP4_RAW`) access unit.
//!
//! The encoder has no `TT_DRM` transport and no Huffman-codeword-reordering (HCR)
//! writer; `decdrm-codecs` converts the raw MPEG-4 access units into DRM access units.
//! The decoder is compiled unmodified and supports `TT_DRM` (AAC, HE-AAC v1/v2 and
//! xHE-AAC/USAC).

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

use core::ffi::{c_char, c_int, c_uint, c_void};
use core::marker::{PhantomData, PhantomPinned};

// ---------------------------------------------------------------------------------------
// machine_type.h
// ---------------------------------------------------------------------------------------

/// `INT` (32-bit on every supported target).
pub type INT = c_int;
/// `UINT`.
pub type UINT = c_uint;
/// `LONG` — 32-bit on both LP64 (it is `#define`d to `INT`) and LLP64 targets.
pub type LONG = i32;
/// `ULONG` — 32-bit, see [`LONG`].
pub type ULONG = u32;
/// `SHORT`.
pub type SHORT = i16;
/// `USHORT`.
pub type USHORT = u16;
/// `SCHAR`.
pub type SCHAR = i8;
/// `UCHAR`.
pub type UCHAR = u8;
/// `INT64`.
pub type INT64 = i64;
/// PCM sample type of the library build (16-bit signed, `SAMPLE_BITS == 16`).
pub type INT_PCM = i16;

// ---------------------------------------------------------------------------------------
// FDK_audio.h
// ---------------------------------------------------------------------------------------

/// `TRANSPORT_TYPE` (C enum, `int`-sized).
pub type TRANSPORT_TYPE = c_int;
pub const TT_UNKNOWN: TRANSPORT_TYPE = -1;
pub const TT_MP4_RAW: TRANSPORT_TYPE = 0;
pub const TT_MP4_ADIF: TRANSPORT_TYPE = 1;
pub const TT_MP4_ADTS: TRANSPORT_TYPE = 2;
pub const TT_MP4_LATM_MCP1: TRANSPORT_TYPE = 6;
pub const TT_MP4_LATM_MCP0: TRANSPORT_TYPE = 7;
pub const TT_MP4_LOAS: TRANSPORT_TYPE = 10;
/// Digital Radio Mondiale: raw access units, configured from SDC entity type 9.
pub const TT_DRM: TRANSPORT_TYPE = 12;

/// `AUDIO_OBJECT_TYPE` (C enum, `int`-sized, has negative values).
pub type AUDIO_OBJECT_TYPE = c_int;
pub const AOT_NONE: AUDIO_OBJECT_TYPE = -1;
pub const AOT_NULL_OBJECT: AUDIO_OBJECT_TYPE = 0;
pub const AOT_AAC_MAIN: AUDIO_OBJECT_TYPE = 1;
pub const AOT_AAC_LC: AUDIO_OBJECT_TYPE = 2;
pub const AOT_AAC_SSR: AUDIO_OBJECT_TYPE = 3;
pub const AOT_AAC_LTP: AUDIO_OBJECT_TYPE = 4;
pub const AOT_SBR: AUDIO_OBJECT_TYPE = 5;
pub const AOT_AAC_SCAL: AUDIO_OBJECT_TYPE = 6;
pub const AOT_CELP: AUDIO_OBJECT_TYPE = 8;
pub const AOT_HVXC: AUDIO_OBJECT_TYPE = 9;
pub const AOT_ER_AAC_LC: AUDIO_OBJECT_TYPE = 17;
pub const AOT_ER_AAC_SCAL: AUDIO_OBJECT_TYPE = 20;
pub const AOT_ER_AAC_LD: AUDIO_OBJECT_TYPE = 23;
pub const AOT_ER_CELP: AUDIO_OBJECT_TYPE = 24;
pub const AOT_ER_HVXC: AUDIO_OBJECT_TYPE = 25;
pub const AOT_PS: AUDIO_OBJECT_TYPE = 29;
pub const AOT_MPEGS: AUDIO_OBJECT_TYPE = 30;
pub const AOT_ER_AAC_ELD: AUDIO_OBJECT_TYPE = 39;
pub const AOT_USAC: AUDIO_OBJECT_TYPE = 42;
pub const AOT_MP2_AAC_LC: AUDIO_OBJECT_TYPE = 129;
pub const AOT_MP2_SBR: AUDIO_OBJECT_TYPE = 132;
/// Virtual AOT for DRM (ER-AAC-SCAL without SBR).
pub const AOT_DRM_AAC: AUDIO_OBJECT_TYPE = 143;
/// Virtual AOT for DRM (ER-AAC-SCAL with SBR).
pub const AOT_DRM_SBR: AUDIO_OBJECT_TYPE = 144;
/// Virtual AOT for DRM (ER-AAC-SCAL with SBR and MPEG-PS).
pub const AOT_DRM_MPEG_PS: AUDIO_OBJECT_TYPE = 145;
/// Virtual AOT for DRM Surround (ER-AAC-SCAL (+SBR) + MPS).
pub const AOT_DRM_SURROUND: AUDIO_OBJECT_TYPE = 146;
/// Virtual AOT for DRM with USAC (xHE-AAC).
pub const AOT_DRM_USAC: AUDIO_OBJECT_TYPE = 147;

/// `CHANNEL_MODE` (C enum).
pub type CHANNEL_MODE = c_int;
pub const MODE_INVALID: CHANNEL_MODE = -1;
pub const MODE_UNKNOWN: CHANNEL_MODE = 0;
/// Mono (one SCE).
pub const MODE_1: CHANNEL_MODE = 1;
/// Stereo (one CPE).
pub const MODE_2: CHANNEL_MODE = 2;
pub const MODE_1_2: CHANNEL_MODE = 3;
pub const MODE_1_2_1: CHANNEL_MODE = 4;
pub const MODE_1_2_2: CHANNEL_MODE = 5;
pub const MODE_1_2_2_1: CHANNEL_MODE = 6;
pub const MODE_1_2_2_2_1: CHANNEL_MODE = 7;
pub const MODE_212: CHANNEL_MODE = 128;

/// `AUDIO_CHANNEL_TYPE` (C enum).
pub type AUDIO_CHANNEL_TYPE = c_int;
pub const ACT_NONE: AUDIO_CHANNEL_TYPE = 0x00;
pub const ACT_FRONT: AUDIO_CHANNEL_TYPE = 0x01;
pub const ACT_SIDE: AUDIO_CHANNEL_TYPE = 0x02;
pub const ACT_BACK: AUDIO_CHANNEL_TYPE = 0x03;
pub const ACT_LFE: AUDIO_CHANNEL_TYPE = 0x04;

/// `FDK_MODULE_ID` (C enum).
pub type FDK_MODULE_ID = c_int;
pub const FDK_NONE: FDK_MODULE_ID = 0;
pub const FDK_TOOLS: FDK_MODULE_ID = 1;
pub const FDK_SYSLIB: FDK_MODULE_ID = 2;
pub const FDK_AACDEC: FDK_MODULE_ID = 3;
pub const FDK_AACENC: FDK_MODULE_ID = 4;
pub const FDK_SBRDEC: FDK_MODULE_ID = 5;
pub const FDK_SBRENC: FDK_MODULE_ID = 6;
pub const FDK_TPDEC: FDK_MODULE_ID = 7;
pub const FDK_TPENC: FDK_MODULE_ID = 8;
pub const FDK_MPSDEC: FDK_MODULE_ID = 9;
pub const FDK_PCMDMX: FDK_MODULE_ID = 31;
pub const FDK_MPSENC: FDK_MODULE_ID = 34;
pub const FDK_TDLIMIT: FDK_MODULE_ID = 35;
pub const FDK_UNIDRCDEC: FDK_MODULE_ID = 38;
/// Number of entries a [`LIB_INFO`] array passed to the `*GetLibInfo` functions must have.
pub const FDK_MODULE_LAST: usize = 39;

/// `EXT_PAYLOAD_TYPE` values used in MPEG-4 fill elements.
pub const EXT_FIL: u32 = 0x00;
pub const EXT_FILL_DATA: u32 = 0x01;
pub const EXT_DATA_ELEMENT: u32 = 0x02;
pub const EXT_DYNAMIC_RANGE: u32 = 0x0b;
pub const EXT_SBR_DATA: u32 = 0x0d;
pub const EXT_SBR_DATA_CRC: u32 = 0x0e;

/// `MP4_ELEMENT_ID` values of a raw_data_block().
pub const ID_SCE: u32 = 0;
pub const ID_CPE: u32 = 1;
pub const ID_CCE: u32 = 2;
pub const ID_LFE: u32 = 3;
pub const ID_DSE: u32 = 4;
pub const ID_PCE: u32 = 5;
pub const ID_FIL: u32 = 6;
pub const ID_END: u32 = 7;

// Bitstream syntax flags (`CStreamInfo::flags`).
pub const AC_ER_VCB11: UINT = 0x000001;
pub const AC_ER_RVLC: UINT = 0x000002;
pub const AC_ER_HCR: UINT = 0x000004;
pub const AC_SCALABLE: UINT = 0x000008;
pub const AC_ELD: UINT = 0x000010;
pub const AC_LD: UINT = 0x000020;
pub const AC_ER: UINT = 0x000040;
pub const AC_BSAC: UINT = 0x000080;
pub const AC_USAC: UINT = 0x000100;
pub const AC_RSV603DA: UINT = 0x000200;
pub const AC_HDAAC: UINT = 0x000400;
pub const AC_RSVD50: UINT = 0x004000;
pub const AC_SBR_PRESENT: UINT = 0x008000;
pub const AC_SBRCRC: UINT = 0x010000;
pub const AC_PS_PRESENT: UINT = 0x020000;
pub const AC_MPS_PRESENT: UINT = 0x040000;
pub const AC_DRM: UINT = 0x080000;
pub const AC_INDEP: UINT = 0x100000;
pub const AC_MPEGD_RES: UINT = 0x200000;
pub const AC_SAOC_PRESENT: UINT = 0x400000;
pub const AC_DAB: UINT = 0x800000;
pub const AC_ELD_DOWNSCALE: UINT = 0x1000000;
pub const AC_LD_MPS: UINT = 0x2000000;
pub const AC_DRC_PRESENT: UINT = 0x4000000;
pub const AC_USAC_SCFGI3: UINT = 0x8000000;

// Capability flags of the AAC decoder/encoder modules (`LIB_INFO::flags`).
pub const CAPF_AAC_LC: UINT = 0x00000001;
pub const CAPF_ER_AAC_LD: UINT = 0x00000002;
pub const CAPF_ER_AAC_SCAL: UINT = 0x00000004;
pub const CAPF_ER_AAC_LC: UINT = 0x00000008;
pub const CAPF_AAC_480: UINT = 0x00000010;
pub const CAPF_AAC_512: UINT = 0x00000020;
pub const CAPF_AAC_960: UINT = 0x00000040;
pub const CAPF_AAC_1024: UINT = 0x00000080;
pub const CAPF_AAC_HCR: UINT = 0x00000100;
pub const CAPF_AAC_VCB11: UINT = 0x00000200;
pub const CAPF_AAC_RVLC: UINT = 0x00000400;
pub const CAPF_AAC_MPEG4: UINT = 0x00000800;
pub const CAPF_AAC_DRC: UINT = 0x00001000;
pub const CAPF_AAC_CONCEALMENT: UINT = 0x00002000;
pub const CAPF_AAC_DRM_BSFORMAT: UINT = 0x00004000;
pub const CAPF_ER_AAC_ELD: UINT = 0x00008000;
pub const CAPF_ER_AAC_BSAC: UINT = 0x00010000;
pub const CAPF_AAC_ELD_DOWNSCALE: UINT = 0x00040000;
pub const CAPF_AAC_USAC_LP: UINT = 0x00100000;
pub const CAPF_AAC_USAC: UINT = 0x00200000;
pub const CAPF_ER_AAC_ELDV2: UINT = 0x00800000;
pub const CAPF_AAC_UNIDRC: UINT = 0x01000000;
// ... of the transport modules.
pub const CAPF_ADTS: UINT = 0x00000001;
pub const CAPF_ADIF: UINT = 0x00000002;
pub const CAPF_LATM: UINT = 0x00000004;
pub const CAPF_LOAS: UINT = 0x00000008;
pub const CAPF_RAWPACKETS: UINT = 0x00000010;
pub const CAPF_DRM: UINT = 0x00000020;
pub const CAPF_RSVD50: UINT = 0x00000040;
// ... of the SBR modules.
pub const CAPF_SBR_LP: UINT = 0x00000001;
pub const CAPF_SBR_HQ: UINT = 0x00000002;
pub const CAPF_SBR_DRM_BS: UINT = 0x00000004;
pub const CAPF_SBR_CONCEALMENT: UINT = 0x00000008;
pub const CAPF_SBR_DRC: UINT = 0x00000010;
pub const CAPF_SBR_PS_MPEG: UINT = 0x00000020;
pub const CAPF_SBR_PS_DRM: UINT = 0x00000040;
pub const CAPF_SBR_ELD_DOWNSCALE: UINT = 0x00000080;
pub const CAPF_SBR_HBEHQ: UINT = 0x00000100;

/// Library information record filled by [`aacDecoder_GetLibInfo`] / [`aacEncGetLibInfo`].
///
/// Callers must pass an array of [`FDK_MODULE_LAST`] records whose `module_id` fields are
/// initialised to [`FDK_NONE`] (the C helper `FDKinitLibInfo()` is a header-only inline).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LIB_INFO {
    pub title: *const c_char,
    pub build_date: *const c_char,
    pub build_time: *const c_char,
    pub module_id: FDK_MODULE_ID,
    pub version: INT,
    pub flags: UINT,
    pub versionStr: [c_char; 32],
}

impl LIB_INFO {
    /// An empty record (`module_id == FDK_NONE`), as `FDKinitLibInfo()` would produce.
    pub const EMPTY: LIB_INFO = LIB_INFO {
        title: core::ptr::null(),
        build_date: core::ptr::null(),
        build_time: core::ptr::null(),
        module_id: FDK_NONE,
        version: 0,
        flags: 0,
        versionStr: [0; 32],
    };
}

// ---------------------------------------------------------------------------------------
// aacdecoder_lib.h
// ---------------------------------------------------------------------------------------

/// `AAC_DECODER_ERROR` (C enum).
pub type AAC_DECODER_ERROR = c_int;
pub const AAC_DEC_OK: AAC_DECODER_ERROR = 0x0000;
pub const AAC_DEC_OUT_OF_MEMORY: AAC_DECODER_ERROR = 0x0002;
pub const AAC_DEC_UNKNOWN: AAC_DECODER_ERROR = 0x0005;
pub const aac_dec_sync_error_start: AAC_DECODER_ERROR = 0x1000;
pub const AAC_DEC_TRANSPORT_SYNC_ERROR: AAC_DECODER_ERROR = 0x1001;
pub const AAC_DEC_NOT_ENOUGH_BITS: AAC_DECODER_ERROR = 0x1002;
pub const aac_dec_sync_error_end: AAC_DECODER_ERROR = 0x1FFF;
pub const aac_dec_init_error_start: AAC_DECODER_ERROR = 0x2000;
pub const AAC_DEC_INVALID_HANDLE: AAC_DECODER_ERROR = 0x2001;
pub const AAC_DEC_UNSUPPORTED_AOT: AAC_DECODER_ERROR = 0x2002;
pub const AAC_DEC_UNSUPPORTED_FORMAT: AAC_DECODER_ERROR = 0x2003;
pub const AAC_DEC_UNSUPPORTED_ER_FORMAT: AAC_DECODER_ERROR = 0x2004;
pub const AAC_DEC_UNSUPPORTED_EPCONFIG: AAC_DECODER_ERROR = 0x2005;
pub const AAC_DEC_UNSUPPORTED_MULTILAYER: AAC_DECODER_ERROR = 0x2006;
pub const AAC_DEC_UNSUPPORTED_CHANNELCONFIG: AAC_DECODER_ERROR = 0x2007;
pub const AAC_DEC_UNSUPPORTED_SAMPLINGRATE: AAC_DECODER_ERROR = 0x2008;
pub const AAC_DEC_INVALID_SBR_CONFIG: AAC_DECODER_ERROR = 0x2009;
pub const AAC_DEC_SET_PARAM_FAIL: AAC_DECODER_ERROR = 0x200A;
pub const AAC_DEC_NEED_TO_RESTART: AAC_DECODER_ERROR = 0x200B;
pub const AAC_DEC_OUTPUT_BUFFER_TOO_SMALL: AAC_DECODER_ERROR = 0x200C;
pub const aac_dec_init_error_end: AAC_DECODER_ERROR = 0x2FFF;
pub const aac_dec_decode_error_start: AAC_DECODER_ERROR = 0x4000;
pub const AAC_DEC_TRANSPORT_ERROR: AAC_DECODER_ERROR = 0x4001;
pub const AAC_DEC_PARSE_ERROR: AAC_DECODER_ERROR = 0x4002;
pub const AAC_DEC_UNSUPPORTED_EXTENSION_PAYLOAD: AAC_DECODER_ERROR = 0x4003;
pub const AAC_DEC_DECODE_FRAME_ERROR: AAC_DECODER_ERROR = 0x4004;
pub const AAC_DEC_CRC_ERROR: AAC_DECODER_ERROR = 0x4005;
pub const AAC_DEC_INVALID_CODE_BOOK: AAC_DECODER_ERROR = 0x4006;
pub const AAC_DEC_UNSUPPORTED_PREDICTION: AAC_DECODER_ERROR = 0x4007;
pub const AAC_DEC_UNSUPPORTED_CCE: AAC_DECODER_ERROR = 0x4008;
pub const AAC_DEC_UNSUPPORTED_LFE: AAC_DECODER_ERROR = 0x4009;
pub const AAC_DEC_UNSUPPORTED_GAIN_CONTROL_DATA: AAC_DECODER_ERROR = 0x400A;
pub const AAC_DEC_UNSUPPORTED_SBA: AAC_DECODER_ERROR = 0x400B;
pub const AAC_DEC_TNS_READ_ERROR: AAC_DECODER_ERROR = 0x400C;
pub const AAC_DEC_RVLC_ERROR: AAC_DECODER_ERROR = 0x400D;
pub const aac_dec_decode_error_end: AAC_DECODER_ERROR = 0x4FFF;
pub const aac_dec_anc_data_error_start: AAC_DECODER_ERROR = 0x8000;
pub const AAC_DEC_ANC_DATA_ERROR: AAC_DECODER_ERROR = 0x8001;
pub const AAC_DEC_TOO_SMALL_ANC_BUFFER: AAC_DECODER_ERROR = 0x8002;
pub const AAC_DEC_TOO_MANY_ANC_ELEMENTS: AAC_DECODER_ERROR = 0x8003;
pub const aac_dec_anc_data_error_end: AAC_DECODER_ERROR = 0x8FFF;

/// `IS_INIT_ERROR()` macro.
pub const fn IS_INIT_ERROR(err: AAC_DECODER_ERROR) -> bool {
    err >= aac_dec_init_error_start && err <= aac_dec_init_error_end
}
/// `IS_DECODE_ERROR()` macro: the output buffer holds concealed audio.
pub const fn IS_DECODE_ERROR(err: AAC_DECODER_ERROR) -> bool {
    err >= aac_dec_decode_error_start && err <= aac_dec_decode_error_end
}
/// `IS_OUTPUT_VALID()` macro.
pub const fn IS_OUTPUT_VALID(err: AAC_DECODER_ERROR) -> bool {
    err == AAC_DEC_OK || IS_DECODE_ERROR(err)
}

/// `AACDEC_PARAM` (C enum).
pub type AACDEC_PARAM = c_int;
pub const AAC_PCM_DUAL_CHANNEL_OUTPUT_MODE: AACDEC_PARAM = 0x0002;
pub const AAC_PCM_OUTPUT_CHANNEL_MAPPING: AACDEC_PARAM = 0x0003;
pub const AAC_PCM_LIMITER_ENABLE: AACDEC_PARAM = 0x0004;
pub const AAC_PCM_LIMITER_ATTACK_TIME: AACDEC_PARAM = 0x0005;
pub const AAC_PCM_LIMITER_RELEAS_TIME: AACDEC_PARAM = 0x0006;
pub const AAC_PCM_MIN_OUTPUT_CHANNELS: AACDEC_PARAM = 0x0011;
pub const AAC_PCM_MAX_OUTPUT_CHANNELS: AACDEC_PARAM = 0x0012;
pub const AAC_METADATA_PROFILE: AACDEC_PARAM = 0x0020;
pub const AAC_METADATA_EXPIRY_TIME: AACDEC_PARAM = 0x0021;
pub const AAC_CONCEAL_METHOD: AACDEC_PARAM = 0x0100;
pub const AAC_DRC_BOOST_FACTOR: AACDEC_PARAM = 0x0200;
pub const AAC_DRC_ATTENUATION_FACTOR: AACDEC_PARAM = 0x0201;
pub const AAC_DRC_REFERENCE_LEVEL: AACDEC_PARAM = 0x0202;
pub const AAC_DRC_HEAVY_COMPRESSION: AACDEC_PARAM = 0x0203;
pub const AAC_DRC_DEFAULT_PRESENTATION_MODE: AACDEC_PARAM = 0x0204;
pub const AAC_DRC_ENC_TARGET_LEVEL: AACDEC_PARAM = 0x0205;
pub const AAC_UNIDRC_SET_EFFECT: AACDEC_PARAM = 0x0206;
pub const AAC_UNIDRC_ALBUM_MODE: AACDEC_PARAM = 0x0207;
pub const AAC_QMF_LOWPOWER: AACDEC_PARAM = 0x0300;
pub const AAC_TPDEC_CLEAR_BUFFER: AACDEC_PARAM = 0x0603;

/// [`aacDecoder_DecodeFrame`] flag: produce concealment output without consuming input.
pub const AACDEC_CONCEAL: UINT = 1;
/// Flag: flush the filter banks (produce the remaining delayed output).
pub const AACDEC_FLUSH: UINT = 2;
/// Flag: signal an input interruption (resynchronise, reset internal buffers).
pub const AACDEC_INTR: UINT = 4;
/// Flag: clear all signal delay lines and history buffers.
pub const AACDEC_CLRHIST: UINT = 8;

/// Stream information returned by [`aacDecoder_GetStreamInfo`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CStreamInfo {
    /// Output sampling rate (after SBR / resampling).
    pub sampleRate: INT,
    /// Output samples per channel of one decoded frame.
    pub frameSize: INT,
    /// Number of output channels.
    pub numChannels: INT,
    pub pChannelType: *mut AUDIO_CHANNEL_TYPE,
    pub pChannelIndices: *mut UCHAR,
    /// Core (AAC) sampling rate.
    pub aacSampleRate: INT,
    pub profile: INT,
    /// Audio object type (for DRM: one of the `AOT_DRM_*` or [`AOT_USAC`]).
    pub aot: AUDIO_OBJECT_TYPE,
    pub channelConfig: INT,
    pub bitRate: INT,
    /// Core samples per frame (960 for DRM AAC).
    pub aacSamplesPerFrame: INT,
    /// Channels coded by the core (1 for mono and parametric stereo).
    pub aacNumChannels: INT,
    /// Extension AOT ([`AOT_SBR`] when SBR is present).
    pub extAot: AUDIO_OBJECT_TYPE,
    pub extSamplingRate: INT,
    /// Decoder output delay in samples at the output rate.
    pub outputDelay: UINT,
    /// `AC_*` bitstream syntax flags.
    pub flags: UINT,
    pub epConfig: SCHAR,
    pub numLostAccessUnits: INT,
    pub numTotalBytes: INT64,
    pub numBadBytes: INT64,
    pub numTotalAccessUnits: INT64,
    pub numBadAccessUnits: INT64,
    pub drcProgRefLev: SCHAR,
    pub drcPresMode: SCHAR,
    pub outputLoudness: INT,
}

/// Opaque decoder instance (`struct AAC_DECODER_INSTANCE`). Never constructed in Rust.
#[repr(C)]
pub struct AAC_DECODER_INSTANCE {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}
/// `HANDLE_AACDECODER`.
pub type HANDLE_AACDECODER = *mut AAC_DECODER_INSTANCE;

// ---------------------------------------------------------------------------------------
// aacenc_lib.h
// ---------------------------------------------------------------------------------------

/// `AACENC_ERROR` (C enum).
pub type AACENC_ERROR = c_int;
pub const AACENC_OK: AACENC_ERROR = 0x0000;
pub const AACENC_INVALID_HANDLE: AACENC_ERROR = 0x0020;
pub const AACENC_MEMORY_ERROR: AACENC_ERROR = 0x0021;
pub const AACENC_UNSUPPORTED_PARAMETER: AACENC_ERROR = 0x0022;
pub const AACENC_INVALID_CONFIG: AACENC_ERROR = 0x0023;
pub const AACENC_INIT_ERROR: AACENC_ERROR = 0x0040;
pub const AACENC_INIT_AAC_ERROR: AACENC_ERROR = 0x0041;
pub const AACENC_INIT_SBR_ERROR: AACENC_ERROR = 0x0042;
pub const AACENC_INIT_TP_ERROR: AACENC_ERROR = 0x0043;
pub const AACENC_INIT_META_ERROR: AACENC_ERROR = 0x0044;
pub const AACENC_INIT_MPS_ERROR: AACENC_ERROR = 0x0045;
pub const AACENC_ENCODE_ERROR: AACENC_ERROR = 0x0060;
pub const AACENC_ENCODE_EOF: AACENC_ERROR = 0x0080;

/// `AACENC_BufferIdentifier` (C enum).
pub type AACENC_BufferIdentifier = c_int;
pub const IN_AUDIO_DATA: AACENC_BufferIdentifier = 0;
pub const IN_ANCILLRY_DATA: AACENC_BufferIdentifier = 1;
pub const IN_METADATA_SETUP: AACENC_BufferIdentifier = 2;
pub const OUT_BITSTREAM_DATA: AACENC_BufferIdentifier = 3;
pub const OUT_AU_SIZES: AACENC_BufferIdentifier = 4;

/// Opaque encoder instance (`struct AACENCODER`). Never constructed in Rust.
#[repr(C)]
pub struct AACENCODER {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}
/// `HANDLE_AACENCODER`.
pub type HANDLE_AACENCODER = *mut AACENCODER;

/// Encoder information filled by [`aacEncInfo`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AACENC_InfoStruct {
    /// Maximum number of encoder bitstream bytes within one frame.
    pub maxOutBufBytes: UINT,
    pub maxAncBytes: UINT,
    pub inBufFillLevel: UINT,
    /// Number of input channels expected in the input buffer.
    pub inputChannels: UINT,
    /// Input samples per channel consumed per encoded frame.
    pub frameLength: UINT,
    /// Codec delay in PCM samples per channel.
    pub nDelay: UINT,
    pub nDelayCore: UINT,
    /// AudioSpecificConfig (for raw transport).
    pub confBuf: [UCHAR; 64],
    pub confSize: UINT,
}

/// Buffer descriptor for [`aacEncEncode`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AACENC_BufDesc {
    pub numBufs: INT,
    pub bufs: *mut *mut c_void,
    pub bufferIdentifiers: *mut INT,
    pub bufSizes: *mut INT,
    pub bufElSizes: *mut INT,
}

/// Input arguments of [`aacEncEncode`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AACENC_InArgs {
    /// Number of valid input samples (all channels); `-1` signals end of input (flush).
    pub numInSamples: INT,
    pub numAncBytes: INT,
}

/// Output arguments of [`aacEncEncode`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AACENC_OutArgs {
    pub numOutBytes: INT,
    pub numInSamples: INT,
    pub numAncBytes: INT,
    pub bitResState: INT,
}

/// `AACENC_CTRLFLAGS`.
pub const AACENC_INIT_NONE: UINT = 0x0000;
pub const AACENC_INIT_CONFIG: UINT = 0x0001;
pub const AACENC_INIT_STATES: UINT = 0x0002;
pub const AACENC_INIT_TRANSPORT: UINT = 0x1000;
pub const AACENC_RESET_INBUFFER: UINT = 0x2000;
pub const AACENC_INIT_ALL: UINT = 0xFFFF;

/// `AACENC_PARAM` (C enum).
pub type AACENC_PARAM = c_int;
pub const AACENC_AOT: AACENC_PARAM = 0x0100;
pub const AACENC_BITRATE: AACENC_PARAM = 0x0101;
pub const AACENC_BITRATEMODE: AACENC_PARAM = 0x0102;
pub const AACENC_SAMPLERATE: AACENC_PARAM = 0x0103;
pub const AACENC_SBR_MODE: AACENC_PARAM = 0x0104;
/// Core frame length. DecDRM's build also accepts 960.
pub const AACENC_GRANULE_LENGTH: AACENC_PARAM = 0x0105;
pub const AACENC_CHANNELMODE: AACENC_PARAM = 0x0106;
pub const AACENC_CHANNELORDER: AACENC_PARAM = 0x0107;
pub const AACENC_SBR_RATIO: AACENC_PARAM = 0x0108;
pub const AACENC_AFTERBURNER: AACENC_PARAM = 0x0200;
pub const AACENC_BANDWIDTH: AACENC_PARAM = 0x0203;
pub const AACENC_PEAK_BITRATE: AACENC_PARAM = 0x0207;
pub const AACENC_TRANSMUX: AACENC_PARAM = 0x0300;
pub const AACENC_HEADER_PERIOD: AACENC_PARAM = 0x0301;
pub const AACENC_SIGNALING_MODE: AACENC_PARAM = 0x0302;
pub const AACENC_TPSUBFRAMES: AACENC_PARAM = 0x0303;
pub const AACENC_AUDIOMUXVER: AACENC_PARAM = 0x0304;
pub const AACENC_PROTECTION: AACENC_PARAM = 0x0306;
pub const AACENC_ANCILLARY_BITRATE: AACENC_PARAM = 0x0500;
pub const AACENC_METADATA_MODE: AACENC_PARAM = 0x0600;
pub const AACENC_CONTROL_STATE: AACENC_PARAM = 0xFF00;
pub const AACENC_NONE: AACENC_PARAM = 0xFFFF;
/// **DecDRM extension** (only in this crate's patched build): `1` makes the SBR encoder
/// write DRM-syntax SBR payloads — scalable-syntax mono element, leading 8-bit DRM SBR
/// CRC, byte-aligned for the fill element — instead of MPEG-4 GA syntax. Default `0`.
pub const AACENC_DECDRM_DRM_SBR: AACENC_PARAM = 0x0F01;

// ---------------------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------------------

unsafe extern "C" {
    // --- decoder ---
    pub fn aacDecoder_AncDataInit(self_: HANDLE_AACDECODER, buffer: *mut UCHAR, size: c_int)
    -> AAC_DECODER_ERROR;
    pub fn aacDecoder_AncDataGet(
        self_: HANDLE_AACDECODER,
        index: c_int,
        ptr: *mut *mut UCHAR,
        size: *mut c_int,
    ) -> AAC_DECODER_ERROR;
    pub fn aacDecoder_SetParam(
        self_: HANDLE_AACDECODER,
        param: AACDEC_PARAM,
        value: INT,
    ) -> AAC_DECODER_ERROR;
    pub fn aacDecoder_GetFreeBytes(self_: HANDLE_AACDECODER, pFreeBytes: *mut UINT)
    -> AAC_DECODER_ERROR;
    /// Opens a decoder; returns NULL on failure.
    pub fn aacDecoder_Open(transportFmt: TRANSPORT_TYPE, nrOfLayers: UINT) -> HANDLE_AACDECODER;
    /// Configures the decoder out of band. For [`TT_DRM`], `conf[0]` is the SDC entity type
    /// 9 body without the short/stream-id nibble (see decdrm-codecs).
    pub fn aacDecoder_ConfigRaw(
        self_: HANDLE_AACDECODER,
        conf: *mut *mut UCHAR,
        length: *const UINT,
    ) -> AAC_DECODER_ERROR;
    pub fn aacDecoder_Fill(
        self_: HANDLE_AACDECODER,
        pBuffer: *mut *mut UCHAR,
        bufferSize: *const UINT,
        bytesValid: *mut UINT,
    ) -> AAC_DECODER_ERROR;
    /// Decodes one frame into interleaved 16-bit PCM (`timeDataSize` = buffer length in
    /// samples).
    pub fn aacDecoder_DecodeFrame(
        self_: HANDLE_AACDECODER,
        pTimeData: *mut INT_PCM,
        timeDataSize: INT,
        flags: UINT,
    ) -> AAC_DECODER_ERROR;
    pub fn aacDecoder_Close(self_: HANDLE_AACDECODER);
    /// Returns a pointer into the decoder instance (valid until the next call / close).
    pub fn aacDecoder_GetStreamInfo(self_: HANDLE_AACDECODER) -> *mut CStreamInfo;
    /// `info` must point to [`FDK_MODULE_LAST`] records initialised to [`LIB_INFO::EMPTY`].
    pub fn aacDecoder_GetLibInfo(info: *mut LIB_INFO) -> INT;

    // --- encoder ---
    pub fn aacEncOpen(
        phAacEncoder: *mut HANDLE_AACENCODER,
        encModules: UINT,
        maxChannels: UINT,
    ) -> AACENC_ERROR;
    pub fn aacEncClose(phAacEncoder: *mut HANDLE_AACENCODER) -> AACENC_ERROR;
    pub fn aacEncEncode(
        hAacEncoder: HANDLE_AACENCODER,
        inBufDesc: *const AACENC_BufDesc,
        outBufDesc: *const AACENC_BufDesc,
        inargs: *const AACENC_InArgs,
        outargs: *mut AACENC_OutArgs,
    ) -> AACENC_ERROR;
    pub fn aacEncInfo(hAacEncoder: HANDLE_AACENCODER, pInfo: *mut AACENC_InfoStruct)
    -> AACENC_ERROR;
    pub fn aacEncoder_SetParam(
        hAacEncoder: HANDLE_AACENCODER,
        param: AACENC_PARAM,
        value: UINT,
    ) -> AACENC_ERROR;
    pub fn aacEncoder_GetParam(hAacEncoder: HANDLE_AACENCODER, param: AACENC_PARAM) -> UINT;
    /// `info` must point to [`FDK_MODULE_LAST`] records initialised to [`LIB_INFO::EMPTY`].
    pub fn aacEncGetLibInfo(info: *mut LIB_INFO) -> AACENC_ERROR;

    // --- DecDRM shim (csrc/decdrm_fdk_shim.cpp) ---
    /// Number of `sizeof`/`offsetof` records exported by the shim.
    pub fn decdrm_fdk_layout_count() -> usize;
    /// Name of record `i` (NUL-terminated, static) and its value; NULL if out of range.
    pub fn decdrm_fdk_layout_entry(i: usize, value: *mut usize) -> *const c_char;
    /// Copies spectral Huffman codebook `cb` (1..=11) in ISO/IEC 14496-3 index order;
    /// returns the number of entries or -1.
    pub fn decdrm_fdk_huffman_spectral(
        cb: c_int,
        codes: *mut u16,
        lens: *mut u8,
        capacity: c_int,
    ) -> c_int;
    /// Copies the scalefactor codebook (121 entries, index = delta + 60); returns 121 or -1.
    pub fn decdrm_fdk_huffman_scalefactor(codes: *mut u32, lens: *mut u8, capacity: c_int)
    -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::CStr;
    use core::mem::{align_of, offset_of, size_of};

    /// All shim records as (name, value).
    fn shim_layout() -> Vec<(String, usize)> {
        // SAFETY: the shim functions only read static tables; the returned names are
        // static NUL-terminated strings.
        unsafe {
            (0..decdrm_fdk_layout_count())
                .map(|i| {
                    let mut v = 0usize;
                    let name = decdrm_fdk_layout_entry(i, &mut v);
                    assert!(!name.is_null());
                    (CStr::from_ptr(name).to_string_lossy().into_owned(), v)
                })
                .collect()
        }
    }

    macro_rules! rust_layout {
        ($($name:expr => $val:expr),* $(,)?) => { vec![$(($name, $val)),*] };
    }

    #[test]
    fn struct_layouts_match_c() {
        let rust: Vec<(&str, usize)> = rust_layout![
            "sizeof INT" => size_of::<INT>(),
            "sizeof UINT" => size_of::<UINT>(),
            "sizeof LONG" => size_of::<LONG>(),
            "sizeof INT64" => size_of::<INT64>(),
            "sizeof INT_PCM" => size_of::<INT_PCM>(),
            "sizeof AUDIO_OBJECT_TYPE" => size_of::<AUDIO_OBJECT_TYPE>(),
            "sizeof TRANSPORT_TYPE" => size_of::<TRANSPORT_TYPE>(),
            "sizeof CHANNEL_MODE" => size_of::<CHANNEL_MODE>(),
            "sizeof AUDIO_CHANNEL_TYPE" => size_of::<AUDIO_CHANNEL_TYPE>(),
            "sizeof FDK_MODULE_ID" => size_of::<FDK_MODULE_ID>(),
            "sizeof AAC_DECODER_ERROR" => size_of::<AAC_DECODER_ERROR>(),
            "sizeof AACDEC_PARAM" => size_of::<AACDEC_PARAM>(),
            "sizeof AACENC_ERROR" => size_of::<AACENC_ERROR>(),
            "sizeof AACENC_PARAM" => size_of::<AACENC_PARAM>(),
            "sizeof AACENC_BufferIdentifier" => size_of::<AACENC_BufferIdentifier>(),

            "sizeof CStreamInfo" => size_of::<CStreamInfo>(),
            "alignof CStreamInfo" => align_of::<CStreamInfo>(),
            "CStreamInfo.sampleRate" => offset_of!(CStreamInfo, sampleRate),
            "CStreamInfo.frameSize" => offset_of!(CStreamInfo, frameSize),
            "CStreamInfo.numChannels" => offset_of!(CStreamInfo, numChannels),
            "CStreamInfo.pChannelType" => offset_of!(CStreamInfo, pChannelType),
            "CStreamInfo.pChannelIndices" => offset_of!(CStreamInfo, pChannelIndices),
            "CStreamInfo.aacSampleRate" => offset_of!(CStreamInfo, aacSampleRate),
            "CStreamInfo.profile" => offset_of!(CStreamInfo, profile),
            "CStreamInfo.aot" => offset_of!(CStreamInfo, aot),
            "CStreamInfo.channelConfig" => offset_of!(CStreamInfo, channelConfig),
            "CStreamInfo.bitRate" => offset_of!(CStreamInfo, bitRate),
            "CStreamInfo.aacSamplesPerFrame" => offset_of!(CStreamInfo, aacSamplesPerFrame),
            "CStreamInfo.aacNumChannels" => offset_of!(CStreamInfo, aacNumChannels),
            "CStreamInfo.extAot" => offset_of!(CStreamInfo, extAot),
            "CStreamInfo.extSamplingRate" => offset_of!(CStreamInfo, extSamplingRate),
            "CStreamInfo.outputDelay" => offset_of!(CStreamInfo, outputDelay),
            "CStreamInfo.flags" => offset_of!(CStreamInfo, flags),
            "CStreamInfo.epConfig" => offset_of!(CStreamInfo, epConfig),
            "CStreamInfo.numLostAccessUnits" => offset_of!(CStreamInfo, numLostAccessUnits),
            "CStreamInfo.numTotalBytes" => offset_of!(CStreamInfo, numTotalBytes),
            "CStreamInfo.numBadBytes" => offset_of!(CStreamInfo, numBadBytes),
            "CStreamInfo.numTotalAccessUnits" => offset_of!(CStreamInfo, numTotalAccessUnits),
            "CStreamInfo.numBadAccessUnits" => offset_of!(CStreamInfo, numBadAccessUnits),
            "CStreamInfo.drcProgRefLev" => offset_of!(CStreamInfo, drcProgRefLev),
            "CStreamInfo.drcPresMode" => offset_of!(CStreamInfo, drcPresMode),
            "CStreamInfo.outputLoudness" => offset_of!(CStreamInfo, outputLoudness),

            "sizeof LIB_INFO" => size_of::<LIB_INFO>(),
            "alignof LIB_INFO" => align_of::<LIB_INFO>(),
            "LIB_INFO.title" => offset_of!(LIB_INFO, title),
            "LIB_INFO.build_date" => offset_of!(LIB_INFO, build_date),
            "LIB_INFO.build_time" => offset_of!(LIB_INFO, build_time),
            "LIB_INFO.module_id" => offset_of!(LIB_INFO, module_id),
            "LIB_INFO.version" => offset_of!(LIB_INFO, version),
            "LIB_INFO.flags" => offset_of!(LIB_INFO, flags),
            "LIB_INFO.versionStr" => offset_of!(LIB_INFO, versionStr),

            "sizeof AACENC_InfoStruct" => size_of::<AACENC_InfoStruct>(),
            "alignof AACENC_InfoStruct" => align_of::<AACENC_InfoStruct>(),
            "AACENC_InfoStruct.maxOutBufBytes" => offset_of!(AACENC_InfoStruct, maxOutBufBytes),
            "AACENC_InfoStruct.maxAncBytes" => offset_of!(AACENC_InfoStruct, maxAncBytes),
            "AACENC_InfoStruct.inBufFillLevel" => offset_of!(AACENC_InfoStruct, inBufFillLevel),
            "AACENC_InfoStruct.inputChannels" => offset_of!(AACENC_InfoStruct, inputChannels),
            "AACENC_InfoStruct.frameLength" => offset_of!(AACENC_InfoStruct, frameLength),
            "AACENC_InfoStruct.nDelay" => offset_of!(AACENC_InfoStruct, nDelay),
            "AACENC_InfoStruct.nDelayCore" => offset_of!(AACENC_InfoStruct, nDelayCore),
            "AACENC_InfoStruct.confBuf" => offset_of!(AACENC_InfoStruct, confBuf),
            "AACENC_InfoStruct.confSize" => offset_of!(AACENC_InfoStruct, confSize),

            "sizeof AACENC_BufDesc" => size_of::<AACENC_BufDesc>(),
            "alignof AACENC_BufDesc" => align_of::<AACENC_BufDesc>(),
            "AACENC_BufDesc.numBufs" => offset_of!(AACENC_BufDesc, numBufs),
            "AACENC_BufDesc.bufs" => offset_of!(AACENC_BufDesc, bufs),
            "AACENC_BufDesc.bufferIdentifiers" => offset_of!(AACENC_BufDesc, bufferIdentifiers),
            "AACENC_BufDesc.bufSizes" => offset_of!(AACENC_BufDesc, bufSizes),
            "AACENC_BufDesc.bufElSizes" => offset_of!(AACENC_BufDesc, bufElSizes),

            "sizeof AACENC_InArgs" => size_of::<AACENC_InArgs>(),
            "alignof AACENC_InArgs" => align_of::<AACENC_InArgs>(),
            "AACENC_InArgs.numInSamples" => offset_of!(AACENC_InArgs, numInSamples),
            "AACENC_InArgs.numAncBytes" => offset_of!(AACENC_InArgs, numAncBytes),
            "sizeof AACENC_OutArgs" => size_of::<AACENC_OutArgs>(),
            "alignof AACENC_OutArgs" => align_of::<AACENC_OutArgs>(),
            "AACENC_OutArgs.numOutBytes" => offset_of!(AACENC_OutArgs, numOutBytes),
            "AACENC_OutArgs.numInSamples" => offset_of!(AACENC_OutArgs, numInSamples),
            "AACENC_OutArgs.numAncBytes" => offset_of!(AACENC_OutArgs, numAncBytes),
            "AACENC_OutArgs.bitResState" => offset_of!(AACENC_OutArgs, bitResState),
        ];
        let c = shim_layout();
        assert_eq!(c.len(), rust.len(), "shim and Rust tables differ in length");
        for ((cname, cval), (rname, rval)) in c.iter().zip(rust.iter()) {
            assert_eq!(cname, rname, "table order mismatch");
            assert_eq!(cval, rval, "layout mismatch for {cname}: C {cval} vs Rust {rval}");
        }
    }

    fn lib_infos(f: unsafe extern "C" fn(*mut LIB_INFO) -> c_int) -> Vec<LIB_INFO> {
        let mut info = [LIB_INFO::EMPTY; FDK_MODULE_LAST];
        // SAFETY: `info` has FDK_MODULE_LAST records initialised to FDK_NONE, as required.
        let r = unsafe { f(info.as_mut_ptr()) };
        assert_eq!(r, 0);
        info.iter().copied().take_while(|i| i.module_id != FDK_NONE).collect()
    }

    fn flags_of(infos: &[LIB_INFO], id: FDK_MODULE_ID) -> UINT {
        infos.iter().find(|i| i.module_id == id).map(|i| i.flags).unwrap_or(0)
    }

    #[test]
    fn decoder_reports_drm_and_usac_capabilities() {
        let infos = lib_infos(aacDecoder_GetLibInfo);
        let aac = flags_of(&infos, FDK_AACDEC);
        assert_ne!(aac & CAPF_AAC_DRM_BSFORMAT, 0, "DRM bitstream format");
        assert_ne!(aac & CAPF_AAC_960, 0, "960-sample frames");
        assert_ne!(aac & CAPF_AAC_HCR, 0, "HCR");
        assert_ne!(aac & CAPF_AAC_VCB11, 0, "VCB11");
        assert_ne!(aac & CAPF_AAC_USAC, 0, "USAC (xHE-AAC)");
        let sbr = flags_of(&infos, FDK_SBRDEC);
        assert_ne!(sbr & CAPF_SBR_DRM_BS, 0, "DRM SBR");
        assert_ne!(sbr & CAPF_SBR_PS_MPEG, 0, "MPEG PS");
        let tp = flags_of(&infos, FDK_TPDEC);
        assert_ne!(tp & CAPF_DRM, 0, "DRM transport");
        let dec = infos.iter().find(|i| i.module_id == FDK_AACDEC).unwrap();
        // SAFETY: the library fills `versionStr` with a NUL-terminated string.
        let v = unsafe { CStr::from_ptr(dec.versionStr.as_ptr()) };
        assert!(v.to_str().unwrap().starts_with("3."), "decoder version {v:?}");
    }

    #[test]
    fn patched_encoder_advertises_960() {
        let infos = lib_infos(aacEncGetLibInfo);
        let enc = flags_of(&infos, FDK_AACENC);
        assert_ne!(enc & CAPF_AAC_960, 0, "DecDRM encoder patch not applied");
    }

    #[test]
    fn encoder_accepts_960_granule_and_drm_sbr_switch() {
        let mut h: HANDLE_AACENCODER = core::ptr::null_mut();
        // SAFETY: valid out-pointer; the handle is closed below.
        unsafe {
            assert_eq!(aacEncOpen(&mut h, 0, 1), AACENC_OK);
            assert_eq!(aacEncoder_SetParam(h, AACENC_AOT, AOT_SBR as UINT), AACENC_OK);
            assert_eq!(aacEncoder_SetParam(h, AACENC_SAMPLERATE, 24000), AACENC_OK);
            assert_eq!(aacEncoder_SetParam(h, AACENC_CHANNELMODE, MODE_1 as UINT), AACENC_OK);
            assert_eq!(aacEncoder_SetParam(h, AACENC_GRANULE_LENGTH, 960), AACENC_OK);
            assert_eq!(aacEncoder_SetParam(h, AACENC_TRANSMUX, TT_MP4_RAW as UINT), AACENC_OK);
            assert_eq!(aacEncoder_SetParam(h, AACENC_BITRATE, 16000), AACENC_OK);
            assert_eq!(aacEncoder_SetParam(h, AACENC_DECDRM_DRM_SBR, 1), AACENC_OK);
            assert_eq!(aacEncoder_GetParam(h, AACENC_DECDRM_DRM_SBR), 1);
            assert_eq!(aacEncoder_SetParam(h, AACENC_DECDRM_DRM_SBR, 7), AACENC_INVALID_CONFIG);
            // Initialise with a NULL call, as documented in aacenc_lib.h.
            let r = aacEncEncode(
                h,
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null_mut(),
            );
            assert_eq!(r, AACENC_OK);
            let mut info: AACENC_InfoStruct = core::mem::zeroed();
            assert_eq!(aacEncInfo(h, &mut info), AACENC_OK);
            // Dual-rate SBR: 960 core samples at 12 kHz = 1920 input samples at 24 kHz.
            assert_eq!(info.frameLength, 1920);
            assert_eq!(aacEncoder_GetParam(h, AACENC_GRANULE_LENGTH), 960);
            assert_eq!(aacEncClose(&mut h), AACENC_OK);
        }
        assert!(h.is_null());
    }

    #[test]
    fn huffman_tables_are_exported() {
        let sizes = [0, 81, 81, 81, 81, 81, 81, 64, 64, 169, 169, 289];
        for cb in 1..=11 {
            let mut codes = [0u16; 289];
            let mut lens = [0u8; 289];
            // SAFETY: buffers hold `capacity` entries.
            let n = unsafe {
                decdrm_fdk_huffman_spectral(cb, codes.as_mut_ptr(), lens.as_mut_ptr(), 289)
            };
            assert_eq!(n, sizes[cb as usize], "codebook {cb}");
            let n = n as usize;
            assert!(lens[..n].iter().all(|&l| (1..=16).contains(&l)), "codebook {cb} lengths");
            // A complete prefix code satisfies Kraft's equality.
            let kraft: f64 = lens[..n].iter().map(|&l| 0.5f64.powi(l as i32)).sum();
            assert!((kraft - 1.0).abs() < 1e-9, "codebook {cb} Kraft sum {kraft}");
        }
        let mut codes = [0u32; 121];
        let mut lens = [0u8; 121];
        // SAFETY: buffers hold 121 entries.
        let n = unsafe {
            decdrm_fdk_huffman_scalefactor(codes.as_mut_ptr(), lens.as_mut_ptr(), 121)
        };
        assert_eq!(n, 121);
        assert_eq!(lens[60], 1, "delta 0 is the 1-bit codeword");
        let kraft: f64 = lens.iter().map(|&l| 0.5f64.powi(l as i32)).sum();
        assert!((kraft - 1.0).abs() < 1e-9);
        // SAFETY: invalid codebook numbers are rejected without touching the buffers.
        let bad = unsafe {
            decdrm_fdk_huffman_spectral(12, codes.as_mut_ptr().cast(), lens.as_mut_ptr(), 121)
        };
        assert_eq!(bad, -1);
    }
}
