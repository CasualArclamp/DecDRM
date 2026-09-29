//! Build and raw FFI bindings for the encoder of the vendored libxaac library
//! (`third_party/libxaac`, tag v0.1.13, Apache-2.0) — the xHE-AAC (MPEG-D USAC) encoder
//! of DecDRM's transmitter.
//!
//! This crate compiles the libxaac *encoder* from source into a static library (see
//! `build.rs`) and exposes a *hand-written* subset of its C API (`encoder/ixheaace_api.h`).
//! There is no bindgen (no libclang on the build machines); instead a small C shim
//! (`csrc/decdrm_xaac_shim.c`) reports `sizeof`/`offsetof` for every transcribed struct
//! and the unit tests below compare them with Rust's view.
//!
//! Everything here is `unsafe` and C-shaped on purpose; `decdrm-codecs` wraps it in a
//! safe API (`XheAacEncoder`). Names follow the C headers (hence the lint allowances).
//!
//! # How the libxaac API works
//!
//! 1. The caller fills an [`ixheaace_input_config`] (codec parameters) and an
//!    [`ixheaace_output_config`] whose only inputs are the allocator callbacks
//!    [`ixheaace_output_config::malloc_xheaace`] / `free_xheaace` (use
//!    [`decdrm_xaac_malloc`] / [`decdrm_xaac_free`]).
//!    [`ixheaace_input_config::pv_drc_cfg`] must point to a zeroed buffer of
//!    [`decdrm_xaac_drc_config_size`] bytes even when DRC is off: `ixheaace_create` writes
//!    through it.
//! 2. [`ixheaace_create`] validates the configuration *in place* (the input config then
//!    holds the values really used), allocates everything through the callbacks, writes
//!    the AudioSpecificConfig (with the UsacConfig) into the output buffer and reports its
//!    length in `i_out_bytes`, and the input frame size in `input_size`.
//! 3. Per frame the caller writes `input_size` bytes of interleaved 16-bit PCM into the
//!    input buffer (`mem_info_table[IA_MEMTYPE_INPUT].mem_ptr`) and calls
//!    [`ixheaace_process`], which leaves one access unit (or nothing) in the output buffer
//!    (`mem_info_table[IA_MEMTYPE_OUTPUT].mem_ptr`, `i_out_bytes`).
//! 4. [`ixheaace_delete`] frees everything allocated in step 2.
//!
//! # DecDRM modifications
//!
//! Four encoder files are compiled with small anchored source patches (see `build.rs`,
//! which explains each one); the result is a modified version of libxaac under the Apache
//! License 2.0, and every patched copy says so at its top:
//!
//! * every USAC bit rate is accepted (v0.1.13 replaces all rates other than 64 and
//!   96 kbit/s by 96 kbit/s), with a floor of 4 kbit/s instead of 8 kbit/s per channel;
//! * the encoder's own USAC fill element is disabled (DecDRM writes its own);
//! * the USAC core bandwidth can be preset through the otherwise unused
//!   `aac_config.bandwidth` and never exceeds the SBR crossover (v0.1.13 always codes the
//!   whole core band, which wastes the bits of low-rate streams);
//! * the USAC threshold in quiet is raised by 15 dB, above the quantisation noise of the
//!   16-bit input (v0.1.13 codes that noise, which broke streams below ≈8 kbit/s per
//!   channel and cores of 24-32 kHz);
//! * the TNS tables of the non-standard core rates 9.6/14.4/19.2 kHz are found through the
//!   standard rate mapping (v0.1.13 rejected those rates);
//! * five accessor functions are appended: [`decdrm_ixheaace_frame_bits`],
//!   [`decdrm_ixheaace_num_preroll_frames`], [`decdrm_ixheaace_core_bandwidth`],
//!   [`decdrm_ixheaace_frame_count`] and [`decdrm_ixheaace_set_next_independency`].

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

use core::ffi::c_char;

// ---------------------------------------------------------------------------------------
// ixheaac_type_def.h
// ---------------------------------------------------------------------------------------

/// `WORD8` (`signed char`).
pub type WORD8 = i8;
/// `UWORD16`.
pub type UWORD16 = u16;
/// `WORD32` (`signed int`).
pub type WORD32 = i32;
/// `UWORD32` (`unsigned int`).
pub type UWORD32 = u32;
/// `FLAG` (`signed int`).
pub type FLAG = i32;
/// `FLOAT32`.
pub type FLOAT32 = f32;
/// `FLOAT64`.
pub type FLOAT64 = f64;
/// `SIZE_T` (`size_t`).
pub type SIZE_T = usize;
/// `pVOID` (`void *`).
pub type pVOID = *mut core::ffi::c_void;
/// `IA_ERRORCODE` (`WORD32`). Negative values (bit 31 set) are fatal.
pub type IA_ERRORCODE = WORD32;

// ---------------------------------------------------------------------------------------
// Constants (ixheaace_api.h, iusace_cnst.h, ixheaac_error_standards.h, impd_drc_uni_drc.h)
// ---------------------------------------------------------------------------------------

/// No error.
pub const IA_NO_ERROR: IA_ERRORCODE = 0;
/// Bit set in every fatal error code (`0x80000000`).
pub const IA_FATAL_ERROR: u32 = 0x8000_0000;

/// `mem_info_table` index of the PCM input buffer.
pub const IA_MEMTYPE_INPUT: usize = 0x02;
/// `mem_info_table` index of the bitstream output buffer.
pub const IA_MEMTYPE_OUTPUT: usize = 0x03;
/// Alignment libxaac requests from the allocator.
pub const DEFAULT_MEM_ALIGN_8: UWORD32 = 8;
/// Length of [`ixheaace_output_config::arr_alloc_memory`].
pub const IXHEAACE_MEM_ALLOC_CNT: usize = 6;

/// Audio object type: AAC-LC.
pub const AOT_AAC_LC: WORD32 = 2;
/// Audio object type: USAC (xHE-AAC).
pub const AOT_USAC: WORD32 = 42;

/// `codec_mode`: switch between frequency-domain (AAC-like) and linear-prediction
/// (ACELP/TCX) coding per frame.
pub const USAC_SWITCHED: WORD32 = 0;
/// `codec_mode`: frequency-domain coding only.
pub const USAC_ONLY_FD: WORD32 = 1;
/// `codec_mode`: linear-prediction coding only.
pub const USAC_ONLY_TD: WORD32 = 2;

/// `ccfl_idx` (= coreSbrFrameLengthIndex): 768-sample core, no SBR (not allowed in DRM).
pub const NO_SBR_CCFL_768: WORD32 = 0;
/// `ccfl_idx`: 1024-sample core, no SBR.
pub const NO_SBR_CCFL_1024: WORD32 = 1;
/// `ccfl_idx`: 768-sample core, 8:3 SBR (2048 output samples).
pub const SBR_8_3: WORD32 = 2;
/// `ccfl_idx`: 1024-sample core, 2:1 SBR (2048 output samples).
pub const SBR_2_1: WORD32 = 3;
/// `ccfl_idx`: 1024-sample core, 4:1 SBR (4096 output samples).
pub const SBR_4_1: WORD32 = 4;

/// `method_def`: program loudness (the value libxaac's test bench uses).
pub const METHOD_DEFINITION_PROGRAM_LOUDNESS: UWORD32 = 1;
/// `measurement_system`: ITU-R BS.1770-3 (the only value libxaac accepts).
pub const MEASUREMENT_SYSTEM_BS_1770_3: UWORD32 = 2;

/// `true` for fatal error codes.
pub const fn ia_is_fatal(err: IA_ERRORCODE) -> bool {
    (err as u32) & IA_FATAL_ERROR != 0
}

/// Symbolic name of a libxaac encoder error code (`encoder/ixheaace_error_codes.h`).
pub fn ia_error_name(err: IA_ERRORCODE) -> &'static str {
    match err as u32 {
        0 => "no error",
        0xFFFF_8000 => "API: memory allocation failed",
        0xFFFF_8001 => "API: unsupported audio object type",
        0x0000_0800 => "config: invalid configuration (non-fatal)",
        0x0000_0801 => "config: bit reservoir size too small (non-fatal)",
        0x0000_0B00 => "config: DRC configuration missing (non-fatal)",
        0xFFFF_8800 => "config: invalid sampling frequency",
        0xFFFF_8801 => "config: invalid number of channels",
        0xFFFF_8804 => "config: invalid PCM word size",
        0xFFFF_8A00 => "config: invalid USAC sampling frequency",
        0xFFFF_8A01 => "config: invalid USAC resampler ratio",
        0xFFFF_8A02 => "config: sampling frequency not allowed in the USAC baseline profile",
        0xFFFF_8B00..=0xFFFF_8B03 => "config: invalid DRC configuration",
        0x0000_1300 | 0x0000_1301 => "init: invalid DRC gain points (non-fatal)",
        0xFFFF_9000 => "init: resampler initialisation failed",
        0xFFFF_9003 => "init: bit rate not supported",
        0xFFFF_9200 => "init: USAC resampler initialisation failed",
        0xFFFF_9201 => "init: USAC bit reservoir too small for the bit rate",
        0xFFFF_9202 => "init: invalid USAC core sampling rate",
        0xFFFF_9203 => "init: invalid USAC element type",
        0xFFFF_9204 => "init: USAC bit buffer initialisation failed",
        0xFFFF_9205 => "init: invalid USAC codec mode",
        0xFFFF_9400..=0xFFFF_9405 => "init: SBR initialisation failed",
        0x0000_1A00 => "encode: quantised spectrum is zero (non-fatal)",
        0x0000_1A01 => "encode: insufficient bit reservoir (non-fatal)",
        0x0000_1C00..=0x0000_1C06 => "encode: eSBR warning (non-fatal)",
        0xFFFF_9800..=0xFFFF_9812 => "encode: fatal error in the core/SBR encoder",
        0xFFFF_9A00..=0xFFFF_9A07 => "encode: fatal error in the USAC encoder",
        _ if ia_is_fatal(err) => "fatal error",
        _ => "non-fatal warning",
    }
}

// ---------------------------------------------------------------------------------------
// ixheaace_api.h structs
// ---------------------------------------------------------------------------------------

/// `ixheaace_mem_info_table`: one of the four buffers (persistent, scratch, input, output)
/// the library allocated.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ixheaace_mem_info_table {
    pub ui_size: UWORD32,
    pub ui_alignment: UWORD32,
    pub ui_type: UWORD32,
    pub mem_ptr: pVOID,
}

/// `ixheaace_aac_enc_config` (AAC tuning; for USAC only `use_tns`, `noise_filling`,
/// `length` and the range-checked fields matter).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ixheaace_aac_enc_config {
    pub sample_rate: WORD32,
    pub bitrate: WORD32,
    pub num_channels_in: WORD32,
    pub num_channels_out: WORD32,
    pub bandwidth: WORD32,
    pub dual_mono: WORD32,
    pub use_tns: WORD32,
    pub noise_filling: WORD32,
    pub use_adts: WORD32,
    pub private_bit: WORD32,
    pub copyright_bit: WORD32,
    pub original_copy_bit: WORD32,
    pub f_no_stereo_preprocessing: WORD32,
    pub inv_quant: WORD32,
    pub full_bandwidth: WORD32,
    pub bitreservoir_size: WORD32,
    /// Total input length in bytes (only used for `expected_frame_count`).
    pub length: WORD32,
}

/// `ixheaace_input_config`: the encoder configuration. `ixheaace_create` corrects invalid
/// values in place.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ixheaace_input_config {
    /// PCM word size in bits; must be 16.
    pub ui_pcm_wd_sz: UWORD32,
    pub i_bitrate: WORD32,
    pub frame_length: WORD32,
    pub frame_cmd_flag: WORD32,
    pub out_bytes_flag: WORD32,
    pub user_tns_flag: WORD32,
    pub user_esbr_flag: WORD32,
    pub aot: WORD32,
    pub i_mps_tree_config: WORD32,
    pub esbr_flag: WORD32,
    pub i_channels: WORD32,
    pub i_samp_freq: UWORD32,
    pub i_native_samp_freq: WORD32,
    pub i_channels_mask: WORD32,
    pub i_num_coupling_chan: WORD32,
    pub i_use_mps: WORD32,
    pub i_use_adts: WORD32,
    pub i_use_es: WORD32,
    pub usac_en: WORD32,
    pub codec_mode: WORD32,
    pub cplx_pred: WORD32,
    pub ccfl_idx: WORD32,
    pub pvc_active: WORD32,
    pub harmonic_sbr: WORD32,
    pub inter_tes_active: WORD32,
    /// Points to an `ia_drc_input_config` ([`decdrm_xaac_drc_config_size`] bytes).
    pub pv_drc_cfg: pVOID,
    pub use_drc_element: FLAG,
    pub drc_frame_size: WORD32,
    pub hq_esbr: WORD32,
    pub write_program_config_element: FLAG,
    pub aac_config: ixheaace_aac_enc_config,
    /// Milliseconds between AudioPreRoll frames; -1 = only at the start.
    pub random_access_interval: WORD32,
    pub method_def: UWORD32,
    pub measured_loudness: FLOAT64,
    pub measurement_system: UWORD32,
    pub sample_peak_level: FLOAT32,
    pub stream_id: UWORD16,
    pub use_delay_adjustment: FLAG,
}

impl ixheaace_input_config {
    /// An all-zero configuration (the starting point libxaac's test bench uses too).
    pub fn zeroed() -> Self {
        // SAFETY: every field is an integer, a float or a raw pointer; all-zero bits are a
        // valid value for each of them.
        unsafe { core::mem::zeroed() }
    }
}

/// `ixheaace_version`: static strings filled by [`ixheaace_get_lib_id_strings`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ixheaace_version {
    pub p_lib_name: *mut WORD8,
    pub p_version_num: *mut WORD8,
}

/// Allocator callback: `(size, alignment) -> pointer` (NULL on failure).
pub type ixheaace_malloc_fn = unsafe extern "C" fn(UWORD32, UWORD32) -> pVOID;
/// Release callback matching [`ixheaace_malloc_fn`].
pub type ixheaace_free_fn = unsafe extern "C" fn(pVOID);

/// `ixheaace_output_config`: allocator callbacks in, buffers and stream facts out.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ixheaace_output_config {
    /// Bytes in the output buffer after `ixheaace_create` (the AudioSpecificConfig) or
    /// `ixheaace_process` (one access unit, 0 while the library withholds a frame).
    pub i_out_bytes: WORD32,
    pub i_bytes_consumed: WORD32,
    pub ui_inp_buf_size: UWORD32,
    pub malloc_count: UWORD32,
    pub ui_rem: SIZE_T,
    pub ui_proc_mem_tabs_size: UWORD32,
    /// The encoder instance, passed to [`ixheaace_process`].
    pub pv_ia_process_api_obj: pVOID,
    pub arr_alloc_memory: [pVOID; IXHEAACE_MEM_ALLOC_CNT],
    /// `Option<fn>` has the same layout as a nullable C function pointer.
    pub malloc_xheaace: Option<ixheaace_malloc_fn>,
    pub free_xheaace: Option<ixheaace_free_fn>,
    pub version: ixheaace_version,
    pub mem_info_table: [ixheaace_mem_info_table; 4],
    /// Bytes of PCM the input buffer must be filled with before each `ixheaace_process`.
    pub input_size: WORD32,
    pub samp_freq: WORD32,
    pub header_samp_freq: WORD32,
    pub audio_profile: WORD32,
    pub down_sampling_ratio: FLOAT32,
    pub expected_frame_count: WORD32,
    pub is_loudness_configured: FLAG,
}

impl ixheaace_output_config {
    /// An all-zero output configuration (no allocator set yet).
    pub fn zeroed() -> Self {
        // SAFETY: integers, floats, raw pointers and `Option<fn>` (None = null) are all
        // valid as zero bits.
        unsafe { core::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------------------

// `unsafe extern` (edition 2024): declaring foreign functions is itself an unchecked
// promise about their signatures, and every call is `unsafe`.
unsafe extern "C" {
    /// Fills an [`ixheaace_version`] with static library name / version strings.
    pub fn ixheaace_get_lib_id_strings(pv_output: pVOID) -> IA_ERRORCODE;
    /// Validates the configuration (in place), allocates and initialises the encoder and
    /// writes the AudioSpecificConfig into the output buffer. On a fatal error it frees
    /// what it allocated.
    pub fn ixheaace_create(pv_input: pVOID, pv_output: pVOID) -> IA_ERRORCODE;
    /// Encodes one frame from the input buffer into the output buffer.
    pub fn ixheaace_process(
        pv_ia_process_api_obj: pVOID,
        pstr_in_cfg: pVOID,
        pstr_out_cfg: pVOID,
    ) -> IA_ERRORCODE;
    /// Frees everything allocated by [`ixheaace_create`] (through `free_xheaace`).
    pub fn ixheaace_delete(pv_output: pVOID) -> IA_ERRORCODE;

    // --- DecDRM additions appended to the patched ixheaace_api.c (see build.rs) ---
    /// Exact bit length of the last `UsacFrame()` before byte alignment and before any
    /// AudioPreRoll wrapping (also valid for withheld start-up frames).
    pub fn decdrm_ixheaace_frame_bits(pv_api_obj: pVOID) -> WORD32;
    /// Number of start-up frames that precede the first AudioPreRoll frame.
    pub fn decdrm_ixheaace_num_preroll_frames(pv_api_obj: pVOID) -> WORD32;
    /// Core coder bandwidth in Hz after initialisation (the preset of
    /// `aac_config.bandwidth`, limited to the SBR crossover and the core Nyquist rate).
    pub fn decdrm_ixheaace_core_bandwidth(pv_api_obj: pVOID) -> WORD32;
    /// USAC frames encoded so far.
    pub fn decdrm_ixheaace_frame_count(pv_api_obj: pVOID) -> WORD32;
    /// Selects usacIndependencyFlag of the next frame (only after the start-up frames).
    pub fn decdrm_ixheaace_set_next_independency(pv_api_obj: pVOID, independent: WORD32);

    // --- DecDRM shim (csrc/decdrm_xaac_shim.c) ---
    /// Number of `sizeof`/`offsetof` records exported by the shim.
    pub fn decdrm_xaac_layout_count() -> usize;
    /// Name of record `i` (NUL-terminated, static) and its value; NULL if out of range.
    pub fn decdrm_xaac_layout_entry(i: usize, value: *mut usize) -> *const c_char;
    /// `sizeof(ia_drc_input_config)`.
    pub fn decdrm_xaac_drc_config_size() -> usize;
    /// Aligned allocator for [`ixheaace_output_config::malloc_xheaace`].
    pub fn decdrm_xaac_malloc(size: UWORD32, alignment: UWORD32) -> pVOID;
    /// Release function for [`ixheaace_output_config::free_xheaace`].
    pub fn decdrm_xaac_free(ptr: pVOID);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::CStr;
    use core::mem::{offset_of, size_of};

    /// All shim records as (name, value).
    fn shim_layout() -> Vec<(String, usize)> {
        // SAFETY: the shim functions only read a static table; the returned names are
        // static NUL-terminated strings.
        unsafe {
            (0..decdrm_xaac_layout_count())
                .map(|i| {
                    let mut v = 0usize;
                    let name = decdrm_xaac_layout_entry(i, &mut v);
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
        type In = ixheaace_input_config;
        type Out = ixheaace_output_config;
        type Aac = ixheaace_aac_enc_config;
        type Mem = ixheaace_mem_info_table;
        type Ver = ixheaace_version;
        let rust: Vec<(&str, usize)> = rust_layout![
            "sizeof WORD32" => size_of::<WORD32>(),
            "sizeof UWORD32" => size_of::<UWORD32>(),
            "sizeof FLAG" => size_of::<FLAG>(),
            "sizeof FLOAT32" => size_of::<FLOAT32>(),
            "sizeof FLOAT64" => size_of::<FLOAT64>(),
            "sizeof UWORD16" => size_of::<UWORD16>(),
            "sizeof SIZE_T" => size_of::<SIZE_T>(),
            "sizeof pVOID" => size_of::<pVOID>(),

            "sizeof ixheaace_mem_info_table" => size_of::<Mem>(),
            "ixheaace_mem_info_table.ui_size" => offset_of!(Mem, ui_size),
            "ixheaace_mem_info_table.ui_alignment" => offset_of!(Mem, ui_alignment),
            "ixheaace_mem_info_table.ui_type" => offset_of!(Mem, ui_type),
            "ixheaace_mem_info_table.mem_ptr" => offset_of!(Mem, mem_ptr),

            "sizeof ixheaace_aac_enc_config" => size_of::<Aac>(),
            "ixheaace_aac_enc_config.sample_rate" => offset_of!(Aac, sample_rate),
            "ixheaace_aac_enc_config.bitrate" => offset_of!(Aac, bitrate),
            "ixheaace_aac_enc_config.num_channels_in" => offset_of!(Aac, num_channels_in),
            "ixheaace_aac_enc_config.num_channels_out" => offset_of!(Aac, num_channels_out),
            "ixheaace_aac_enc_config.bandwidth" => offset_of!(Aac, bandwidth),
            "ixheaace_aac_enc_config.dual_mono" => offset_of!(Aac, dual_mono),
            "ixheaace_aac_enc_config.use_tns" => offset_of!(Aac, use_tns),
            "ixheaace_aac_enc_config.noise_filling" => offset_of!(Aac, noise_filling),
            "ixheaace_aac_enc_config.use_adts" => offset_of!(Aac, use_adts),
            "ixheaace_aac_enc_config.private_bit" => offset_of!(Aac, private_bit),
            "ixheaace_aac_enc_config.copyright_bit" => offset_of!(Aac, copyright_bit),
            "ixheaace_aac_enc_config.original_copy_bit" => offset_of!(Aac, original_copy_bit),
            "ixheaace_aac_enc_config.f_no_stereo_preprocessing" =>
                offset_of!(Aac, f_no_stereo_preprocessing),
            "ixheaace_aac_enc_config.inv_quant" => offset_of!(Aac, inv_quant),
            "ixheaace_aac_enc_config.full_bandwidth" => offset_of!(Aac, full_bandwidth),
            "ixheaace_aac_enc_config.bitreservoir_size" => offset_of!(Aac, bitreservoir_size),
            "ixheaace_aac_enc_config.length" => offset_of!(Aac, length),

            "sizeof ixheaace_input_config" => size_of::<In>(),
            "ixheaace_input_config.ui_pcm_wd_sz" => offset_of!(In, ui_pcm_wd_sz),
            "ixheaace_input_config.i_bitrate" => offset_of!(In, i_bitrate),
            "ixheaace_input_config.frame_length" => offset_of!(In, frame_length),
            "ixheaace_input_config.frame_cmd_flag" => offset_of!(In, frame_cmd_flag),
            "ixheaace_input_config.out_bytes_flag" => offset_of!(In, out_bytes_flag),
            "ixheaace_input_config.user_tns_flag" => offset_of!(In, user_tns_flag),
            "ixheaace_input_config.user_esbr_flag" => offset_of!(In, user_esbr_flag),
            "ixheaace_input_config.aot" => offset_of!(In, aot),
            "ixheaace_input_config.i_mps_tree_config" => offset_of!(In, i_mps_tree_config),
            "ixheaace_input_config.esbr_flag" => offset_of!(In, esbr_flag),
            "ixheaace_input_config.i_channels" => offset_of!(In, i_channels),
            "ixheaace_input_config.i_samp_freq" => offset_of!(In, i_samp_freq),
            "ixheaace_input_config.i_native_samp_freq" => offset_of!(In, i_native_samp_freq),
            "ixheaace_input_config.i_channels_mask" => offset_of!(In, i_channels_mask),
            "ixheaace_input_config.i_num_coupling_chan" => offset_of!(In, i_num_coupling_chan),
            "ixheaace_input_config.i_use_mps" => offset_of!(In, i_use_mps),
            "ixheaace_input_config.i_use_adts" => offset_of!(In, i_use_adts),
            "ixheaace_input_config.i_use_es" => offset_of!(In, i_use_es),
            "ixheaace_input_config.usac_en" => offset_of!(In, usac_en),
            "ixheaace_input_config.codec_mode" => offset_of!(In, codec_mode),
            "ixheaace_input_config.cplx_pred" => offset_of!(In, cplx_pred),
            "ixheaace_input_config.ccfl_idx" => offset_of!(In, ccfl_idx),
            "ixheaace_input_config.pvc_active" => offset_of!(In, pvc_active),
            "ixheaace_input_config.harmonic_sbr" => offset_of!(In, harmonic_sbr),
            "ixheaace_input_config.inter_tes_active" => offset_of!(In, inter_tes_active),
            "ixheaace_input_config.pv_drc_cfg" => offset_of!(In, pv_drc_cfg),
            "ixheaace_input_config.use_drc_element" => offset_of!(In, use_drc_element),
            "ixheaace_input_config.drc_frame_size" => offset_of!(In, drc_frame_size),
            "ixheaace_input_config.hq_esbr" => offset_of!(In, hq_esbr),
            "ixheaace_input_config.write_program_config_element" =>
                offset_of!(In, write_program_config_element),
            "ixheaace_input_config.aac_config" => offset_of!(In, aac_config),
            "ixheaace_input_config.random_access_interval" =>
                offset_of!(In, random_access_interval),
            "ixheaace_input_config.method_def" => offset_of!(In, method_def),
            "ixheaace_input_config.measured_loudness" => offset_of!(In, measured_loudness),
            "ixheaace_input_config.measurement_system" => offset_of!(In, measurement_system),
            "ixheaace_input_config.sample_peak_level" => offset_of!(In, sample_peak_level),
            "ixheaace_input_config.stream_id" => offset_of!(In, stream_id),
            "ixheaace_input_config.use_delay_adjustment" => offset_of!(In, use_delay_adjustment),

            "sizeof ixheaace_version" => size_of::<Ver>(),
            "ixheaace_version.p_lib_name" => offset_of!(Ver, p_lib_name),
            "ixheaace_version.p_version_num" => offset_of!(Ver, p_version_num),

            "sizeof ixheaace_output_config" => size_of::<Out>(),
            "ixheaace_output_config.i_out_bytes" => offset_of!(Out, i_out_bytes),
            "ixheaace_output_config.i_bytes_consumed" => offset_of!(Out, i_bytes_consumed),
            "ixheaace_output_config.ui_inp_buf_size" => offset_of!(Out, ui_inp_buf_size),
            "ixheaace_output_config.malloc_count" => offset_of!(Out, malloc_count),
            "ixheaace_output_config.ui_rem" => offset_of!(Out, ui_rem),
            "ixheaace_output_config.ui_proc_mem_tabs_size" => offset_of!(Out, ui_proc_mem_tabs_size),
            "ixheaace_output_config.pv_ia_process_api_obj" => offset_of!(Out, pv_ia_process_api_obj),
            "ixheaace_output_config.arr_alloc_memory" => offset_of!(Out, arr_alloc_memory),
            "ixheaace_output_config.malloc_xheaace" => offset_of!(Out, malloc_xheaace),
            "ixheaace_output_config.free_xheaace" => offset_of!(Out, free_xheaace),
            "ixheaace_output_config.version" => offset_of!(Out, version),
            "ixheaace_output_config.mem_info_table" => offset_of!(Out, mem_info_table),
            "ixheaace_output_config.input_size" => offset_of!(Out, input_size),
            "ixheaace_output_config.samp_freq" => offset_of!(Out, samp_freq),
            "ixheaace_output_config.header_samp_freq" => offset_of!(Out, header_samp_freq),
            "ixheaace_output_config.audio_profile" => offset_of!(Out, audio_profile),
            "ixheaace_output_config.down_sampling_ratio" => offset_of!(Out, down_sampling_ratio),
            "ixheaace_output_config.expected_frame_count" => offset_of!(Out, expected_frame_count),
            "ixheaace_output_config.is_loudness_configured" =>
                offset_of!(Out, is_loudness_configured),
        ];
        let c = shim_layout();
        assert_eq!(c.len(), rust.len(), "shim and Rust tables differ in length");
        for ((cname, cval), (rname, rval)) in c.iter().zip(rust.iter()) {
            assert_eq!(cname, rname, "table order mismatch");
            assert_eq!(
                cval, rval,
                "layout mismatch for {cname}: C {cval} vs Rust {rval}"
            );
        }
    }

    #[test]
    fn library_identifies_itself() {
        let mut v = ixheaace_version {
            p_lib_name: core::ptr::null_mut(),
            p_version_num: core::ptr::null_mut(),
        };
        // SAFETY: valid out-pointer to an ixheaace_version.
        let err = unsafe { ixheaace_get_lib_id_strings((&raw mut v).cast()) };
        assert_eq!(err, IA_NO_ERROR);
        assert!(!v.p_lib_name.is_null() && !v.p_version_num.is_null());
        // SAFETY: static NUL-terminated strings of the library.
        let (name, version) = unsafe {
            (
                CStr::from_ptr(v.p_lib_name.cast())
                    .to_string_lossy()
                    .into_owned(),
                CStr::from_ptr(v.p_version_num.cast())
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        assert!(!name.is_empty(), "library name");
        assert!(!version.is_empty(), "library version");
    }

    #[test]
    fn allocator_is_aligned_and_releasable() {
        for align in [8u32, 16, 64] {
            // SAFETY: plain allocation; freed below.
            let p = unsafe { decdrm_xaac_malloc(1000, align) };
            assert!(!p.is_null());
            assert_eq!(p as usize % align as usize, 0);
            // SAFETY: allocated by decdrm_xaac_malloc, freed once.
            unsafe { decdrm_xaac_free(p) };
        }
        // SAFETY: invalid alignments are rejected without allocating.
        assert!(unsafe { decdrm_xaac_malloc(16, 24) }.is_null());
        // SAFETY: freeing NULL is a no-op.
        unsafe { decdrm_xaac_free(core::ptr::null_mut()) };
        // SAFETY: pure function.
        assert!(unsafe { decdrm_xaac_drc_config_size() } > 1000);
    }

    /// A USAC encoder configuration as DecDRM uses it. The returned DRC buffer backs
    /// `pv_drc_cfg` and must outlive the encoder (moving the `Vec` does not move its heap
    /// buffer).
    fn usac_config(
        rate: u32,
        channels: i32,
        bitrate: i32,
        ccfl_idx: WORD32,
        bandwidth: i32,
    ) -> (ixheaace_input_config, ixheaace_output_config, Vec<u64>) {
        // SAFETY: pure function.
        let drc_size = unsafe { decdrm_xaac_drc_config_size() };
        let mut drc = vec![0u64; drc_size.div_ceil(8)];
        let mut inp = ixheaace_input_config::zeroed();
        inp.ui_pcm_wd_sz = 16;
        inp.aot = AOT_USAC;
        inp.usac_en = 1;
        inp.codec_mode = USAC_ONLY_FD;
        inp.ccfl_idx = ccfl_idx;
        inp.i_channels = channels;
        inp.i_samp_freq = rate;
        inp.i_bitrate = bitrate;
        inp.i_use_es = 1;
        inp.i_mps_tree_config = -1;
        inp.pv_drc_cfg = drc.as_mut_ptr().cast();
        inp.aac_config.inv_quant = 2;
        inp.aac_config.use_tns = 1;
        inp.aac_config.bandwidth = bandwidth;
        inp.random_access_interval = -1;
        inp.method_def = METHOD_DEFINITION_PROGRAM_LOUDNESS;
        inp.measurement_system = MEASUREMENT_SYSTEM_BS_1770_3;
        inp.use_delay_adjustment = 1;
        let mut out = ixheaace_output_config::zeroed();
        out.malloc_xheaace = Some(decdrm_xaac_malloc);
        out.free_xheaace = Some(decdrm_xaac_free);
        (inp, out, drc)
    }

    /// End-to-end smoke test of the raw API and of the build patches: a 12 kbit/s stereo
    /// USAC encoder (rejected by unpatched v0.1.13) is created, keeps its bit rate, limits
    /// its core to the SBR crossover, and produces frames whose exact bit length the DecDRM
    /// accessor reports.
    #[test]
    fn patched_encoder_keeps_low_stereo_bit_rate() {
        let (mut inp, mut out, _drc) = usac_config(24_000, 2, 12_000, SBR_2_1, 0);
        // SAFETY: both configs and the DRC buffer are valid for the duration of the calls;
        // everything allocated is freed by ixheaace_delete below.
        unsafe {
            let err = ixheaace_create((&raw mut inp).cast(), (&raw mut out).cast());
            assert!(!ia_is_fatal(err), "create: {err:#x} {}", ia_error_name(err));
            assert_eq!(inp.i_bitrate, 12_000, "bit rate patch not applied");
            assert_eq!(inp.ccfl_idx, SBR_2_1);
            // 2048 stereo 16-bit samples per frame.
            assert_eq!(out.input_size, 2048 * 2 * 2);
            assert!(out.i_out_bytes > 4, "AudioSpecificConfig expected");
            let obj = out.pv_ia_process_api_obj;
            // Bandwidth patch: the core stops at the SBR crossover, below its 6 kHz Nyquist.
            let bw = decdrm_ixheaace_core_bandwidth(obj);
            assert!((1_000..6_000).contains(&bw), "core bandwidth {bw} Hz");
            let npf = decdrm_ixheaace_num_preroll_frames(obj);
            assert!(npf >= 1);
            let inbuf = out.mem_info_table[IA_MEMTYPE_INPUT].mem_ptr.cast::<i16>();
            let n = out.input_size as usize / 2;
            for f in 0..(npf as usize + 4) {
                for i in 0..n {
                    let t = (f * n + i) / 2;
                    let v = (8000.0 * (t as f64 * 0.1).sin()) as i16;
                    inbuf.add(i).write(v);
                }
                let err = ixheaace_process(obj, (&raw mut inp).cast(), (&raw mut out).cast());
                assert!(
                    !ia_is_fatal(err),
                    "process: {err:#x} {}",
                    ia_error_name(err)
                );
                let bits = decdrm_ixheaace_frame_bits(obj);
                assert!(bits > 8, "frame {f}: {bits} bits");
                if (f as i32) < npf {
                    assert_eq!(out.i_out_bytes, 0, "start-up frame {f} is withheld");
                } else {
                    assert!(out.i_out_bytes > 0, "frame {f} is output");
                }
                assert_eq!(decdrm_ixheaace_frame_count(obj), f as i32 + 1);
                if f as i32 > npf {
                    decdrm_ixheaace_set_next_independency(obj, (f % 2) as i32);
                }
            }
            assert_eq!(ixheaace_delete((&raw mut out).cast()), IA_NO_ERROR);
        }
        assert_eq!(out.malloc_count, 0);
    }

    /// The bandwidth preset (patch 4/6) and the TNS rate mapping (patch 7): a 19.2 kHz core
    /// without SBR (rejected by unpatched v0.1.13) with a 3 kHz core bandwidth.
    #[test]
    fn patched_encoder_accepts_19k2_core_and_bandwidth_preset() {
        let (mut inp, mut out, _drc) = usac_config(19_200, 1, 12_000, NO_SBR_CCFL_1024, 3_000);
        // SAFETY: as above.
        unsafe {
            let err = ixheaace_create((&raw mut inp).cast(), (&raw mut out).cast());
            assert!(!ia_is_fatal(err), "create: {err:#x} {}", ia_error_name(err));
            assert_eq!(
                decdrm_ixheaace_core_bandwidth(out.pv_ia_process_api_obj),
                3_000
            );
            assert_eq!(ixheaace_delete((&raw mut out).cast()), IA_NO_ERROR);
        }
    }
}
