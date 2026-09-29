//! Safe wrappers around FDK-AAC: [`FdkDrmDecoder`] (AAC, HE-AAC v1/v2, xHE-AAC through the
//! DRM transport `TT_DRM`) and [`FdkDrmEncoder`] (AAC, HE-AAC, HE-AAC v2 producing DRM
//! access units).
//!
//! # Ownership of the C handles
//!
//! Each wrapper owns exactly one FDK instance, created in `new` and destroyed in its
//! [`Drop`] implementation — Rust calls `drop` automatically when the value goes out of
//! scope (or is dropped explicitly), so the handle can neither leak nor be closed twice,
//! and no method can observe a closed handle.
//!
//! The raw handle is a C pointer, which makes the wrapper neither `Send` nor `Sync` by
//! default. It is marked `Send` by hand (`unsafe impl Send`): FDK instances keep all state
//! in memory owned by the instance (the library's global data is read-only tables), so an
//! instance may be *moved* to another thread — e.g. created on the GUI thread and used on
//! the audio thread. It is deliberately **not** `Sync`: the library does no locking, and
//! `Sync` would allow two threads to use one instance through shared references at the
//! same time. Share a decoder between threads by wrapping it in a `Mutex`.

use std::ffi::CStr;
use std::ptr::NonNull;

use decdrm_fdk_sys as ffi;

use crate::aac::{self, sfb_table_960};
use crate::bits::BitBuf;
use crate::sdc::{AudioCodingField, AudioInfo, AudioMode};
use crate::{CodecError, DrmAudioDecoder, PcmFrame};

/// Capacity of the decoder output buffer in samples (all channels): comfortably above the
/// largest USAC frame (4096 samples × 2 channels after SBR) FDK can produce for DRM.
const OUT_CAPACITY: usize = 4096 * 8;

fn fdk_error_name(code: i32) -> &'static str {
    match code {
        ffi::AAC_DEC_OUT_OF_MEMORY => "out of memory",
        ffi::AAC_DEC_UNKNOWN => "unknown",
        ffi::AAC_DEC_TRANSPORT_SYNC_ERROR => "transport sync error",
        ffi::AAC_DEC_NOT_ENOUGH_BITS => "not enough bits",
        ffi::AAC_DEC_INVALID_HANDLE => "invalid handle",
        ffi::AAC_DEC_UNSUPPORTED_AOT => "unsupported audio object type",
        ffi::AAC_DEC_UNSUPPORTED_FORMAT => "unsupported format",
        ffi::AAC_DEC_UNSUPPORTED_ER_FORMAT => "unsupported ER format",
        ffi::AAC_DEC_UNSUPPORTED_EPCONFIG => "unsupported epConfig",
        ffi::AAC_DEC_UNSUPPORTED_MULTILAYER => "unsupported multilayer",
        ffi::AAC_DEC_UNSUPPORTED_CHANNELCONFIG => "unsupported channel configuration",
        ffi::AAC_DEC_UNSUPPORTED_SAMPLINGRATE => "unsupported sampling rate",
        ffi::AAC_DEC_INVALID_SBR_CONFIG => "invalid SBR configuration",
        ffi::AAC_DEC_SET_PARAM_FAIL => "set parameter failed",
        ffi::AAC_DEC_NEED_TO_RESTART => "decoder needs restart",
        ffi::AAC_DEC_OUTPUT_BUFFER_TOO_SMALL => "output buffer too small",
        ffi::AAC_DEC_TRANSPORT_ERROR => "transport error",
        ffi::AAC_DEC_PARSE_ERROR => "parse error",
        ffi::AAC_DEC_UNSUPPORTED_EXTENSION_PAYLOAD => "unsupported extension payload",
        ffi::AAC_DEC_DECODE_FRAME_ERROR => "decode frame error",
        ffi::AAC_DEC_CRC_ERROR => "CRC error",
        ffi::AAC_DEC_INVALID_CODE_BOOK => "invalid codebook",
        ffi::AAC_DEC_UNSUPPORTED_PREDICTION => "unsupported prediction",
        ffi::AAC_DEC_UNSUPPORTED_CCE => "unsupported CCE",
        ffi::AAC_DEC_UNSUPPORTED_LFE => "unsupported LFE",
        ffi::AAC_DEC_UNSUPPORTED_GAIN_CONTROL_DATA => "unsupported gain control data",
        ffi::AAC_DEC_UNSUPPORTED_SBA => "unsupported SBA",
        ffi::AAC_DEC_TNS_READ_ERROR => "TNS read error",
        ffi::AAC_DEC_RVLC_ERROR => "RVLC error",
        ffi::AAC_DEC_ANC_DATA_ERROR => "ancillary data error",
        _ => "error",
    }
}

fn dec_err(code: i32, context: &'static str) -> CodecError {
    CodecError::Fdk { code, name: fdk_error_name(code), context }
}

fn enc_err(code: i32, context: &'static str) -> CodecError {
    let name = match code {
        ffi::AACENC_INVALID_HANDLE => "invalid handle",
        ffi::AACENC_MEMORY_ERROR => "memory error",
        ffi::AACENC_UNSUPPORTED_PARAMETER => "unsupported parameter",
        ffi::AACENC_INVALID_CONFIG => "invalid configuration",
        ffi::AACENC_INIT_ERROR => "init error",
        ffi::AACENC_INIT_AAC_ERROR => "AAC init error",
        ffi::AACENC_INIT_SBR_ERROR => "SBR init error",
        ffi::AACENC_INIT_TP_ERROR => "transport init error",
        ffi::AACENC_INIT_META_ERROR => "metadata init error",
        ffi::AACENC_INIT_MPS_ERROR => "MPS init error",
        ffi::AACENC_ENCODE_ERROR => "encode error",
        ffi::AACENC_ENCODE_EOF => "end of file",
        _ => "error",
    };
    CodecError::Fdk { code, name, context }
}

/// Version and DRM-relevant capabilities of the linked FDK-AAC library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdkLibInfo {
    /// AAC decoder version, e.g. `"3.2.0"`.
    pub decoder_version: String,
    /// AAC encoder version, e.g. `"4.0.1"`.
    pub encoder_version: String,
    /// DRM bitstream format (`CAPF_AAC_DRM_BSFORMAT`).
    pub drm: bool,
    /// DRM SBR (`CAPF_SBR_DRM_BS`).
    pub drm_sbr: bool,
    /// MPEG parametric stereo (`CAPF_SBR_PS_MPEG`), used by DRM's PS mode.
    pub parametric_stereo: bool,
    /// USAC, i.e. xHE-AAC (`CAPF_AAC_USAC`).
    pub usac: bool,
    /// The encoder accepts 960-sample frames (DecDRM build patch).
    pub encoder_960: bool,
}

fn lib_infos(f: unsafe extern "C" fn(*mut ffi::LIB_INFO) -> i32) -> Vec<ffi::LIB_INFO> {
    let mut info = [ffi::LIB_INFO::EMPTY; ffi::FDK_MODULE_LAST];
    // SAFETY: `info` holds FDK_MODULE_LAST records initialised to FDK_NONE, as the
    // *_GetLibInfo functions require.
    unsafe { f(info.as_mut_ptr()) };
    info.iter().copied().take_while(|i| i.module_id != ffi::FDK_NONE).collect()
}

fn module(infos: &[ffi::LIB_INFO], id: ffi::FDK_MODULE_ID) -> Option<ffi::LIB_INFO> {
    infos.iter().copied().find(|i| i.module_id == id)
}

fn version_str(i: &ffi::LIB_INFO) -> String {
    // SAFETY: FDK fills `versionStr` with a NUL-terminated string (and the array is
    // zero-initialised).
    unsafe { CStr::from_ptr(i.versionStr.as_ptr()) }.to_string_lossy().into_owned()
}

/// Queries the linked FDK-AAC library.
pub fn fdk_lib_info() -> FdkLibInfo {
    let dec = lib_infos(ffi::aacDecoder_GetLibInfo);
    let enc = lib_infos(ffi::aacEncGetLibInfo);
    let flags = |infos: &[ffi::LIB_INFO], id| module(infos, id).map(|i| i.flags).unwrap_or(0);
    let aac = flags(&dec, ffi::FDK_AACDEC);
    let sbr = flags(&dec, ffi::FDK_SBRDEC);
    FdkLibInfo {
        decoder_version: module(&dec, ffi::FDK_AACDEC).map(|i| version_str(&i)).unwrap_or_default(),
        encoder_version: module(&enc, ffi::FDK_AACENC).map(|i| version_str(&i)).unwrap_or_default(),
        drm: aac & ffi::CAPF_AAC_DRM_BSFORMAT != 0,
        drm_sbr: sbr & ffi::CAPF_SBR_DRM_BS != 0,
        parametric_stereo: sbr & ffi::CAPF_SBR_PS_MPEG != 0,
        usac: aac & ffi::CAPF_AAC_USAC != 0,
        encoder_960: flags(&enc, ffi::FDK_AACENC) & ffi::CAPF_AAC_960 != 0,
    }
}

/// The parts of FDK's `CStreamInfo` that DecDRM uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacStreamInfo {
    /// Output sampling rate in Hz.
    pub sample_rate: u32,
    /// Output samples per channel per frame.
    pub frame_size: usize,
    /// Output channels.
    pub channels: u16,
    /// Core (AAC) sampling rate in Hz.
    pub core_sample_rate: u32,
    /// Channels coded by the core (1 for parametric stereo).
    pub core_channels: u16,
    /// SBR active.
    pub sbr: bool,
    /// Parametric stereo active.
    pub ps: bool,
    /// USAC (xHE-AAC).
    pub usac: bool,
    /// Decoder delay in output samples.
    pub output_delay: u32,
}

/// DRM AAC / xHE-AAC decoder (FDK-AAC with transport `TT_DRM`), configured from the SDC
/// type-9 audio information exactly as Dream's `FdkAacCodec::DecOpen()` does.
pub struct FdkDrmDecoder {
    handle: NonNull<ffi::AAC_DECODER_INSTANCE>,
    info: AudioInfo,
    usac: bool,
    input: Vec<u8>,
    out: Vec<i16>,
    /// Geometry of the last output (used for silence when FDK cannot conceal yet).
    last: Option<(u32, u16, usize)>,
}

// SAFETY: see the module docs — the instance is exclusively owned and FDK keeps no
// thread-affine or shared mutable global state, so moving it between threads is sound.
unsafe impl Send for FdkDrmDecoder {}

impl Drop for FdkDrmDecoder {
    fn drop(&mut self) {
        // SAFETY: the handle came from aacDecoder_Open and is closed exactly once here.
        unsafe { ffi::aacDecoder_Close(self.handle.as_ptr()) }
    }
}

impl FdkDrmDecoder {
    /// Opens a decoder for an AAC or xHE-AAC service.
    pub fn new(info: &AudioInfo) -> Result<Self, CodecError> {
        let usac = match info.coding {
            AudioCodingField::Aac if info.sample_rate_code != 7 => false,
            AudioCodingField::XheAac => true,
            _ => {
                return Err(CodecError::Unsupported(format!(
                    "FDK decoder cannot decode audio coding {:?}",
                    info.coding
                )));
            }
        };
        // SAFETY: plain constructor call; NULL is handled below.
        let raw = unsafe { ffi::aacDecoder_Open(ffi::TT_DRM, 1) };
        let handle = NonNull::new(raw)
            .ok_or(CodecError::Fdk { code: 0, name: "aacDecoder_Open failed", context: "open" })?;
        let mut dec = FdkDrmDecoder {
            handle,
            info: info.clone(),
            usac,
            input: Vec::with_capacity(2048),
            out: vec![0; OUT_CAPACITY],
            last: None,
        };
        let mut conf = info.to_type9_bytes();
        let mut ptr = conf.as_mut_ptr();
        let len = conf.len() as u32;
        // SAFETY: `ptr`/`len` describe the live `conf` buffer; FDK copies the data.
        let err = unsafe { ffi::aacDecoder_ConfigRaw(dec.handle.as_ptr(), &mut ptr, &len) };
        if err != ffi::AAC_DEC_OK {
            return Err(dec_err(err, "configuring from SDC type 9"));
        }
        // Mono/stereo output only (a DRM surround service is downmixed).
        // SAFETY: valid handle; plain integer parameter.
        let err = unsafe {
            ffi::aacDecoder_SetParam(dec.handle.as_ptr(), ffi::AAC_PCM_MAX_OUTPUT_CHANNELS, 2)
        };
        if err != ffi::AAC_DEC_OK {
            return Err(dec_err(err, "limiting output channels"));
        }
        dec.last = dec.nominal_geometry();
        Ok(dec)
    }

    /// Opens a decoder from type-9 bytes (see [`crate::sdc`]).
    pub fn from_type9_bytes(bytes: &[u8]) -> Result<Self, CodecError> {
        Self::new(&AudioInfo::from_type9_bytes(bytes)?)
    }

    /// The audio information the decoder was configured with.
    pub fn audio_info(&self) -> &AudioInfo {
        &self.info
    }

    fn raw_stream_info(&self) -> Option<ffi::CStreamInfo> {
        // SAFETY: valid handle; the returned pointer points into the instance and is only
        // dereferenced (copied) immediately, while `self` is borrowed.
        let p = unsafe { ffi::aacDecoder_GetStreamInfo(self.handle.as_ptr()) };
        if p.is_null() {
            None
        } else {
            // SAFETY: non-null pointer to a CStreamInfo inside the live instance.
            Some(unsafe { *p })
        }
    }

    /// Stream information as currently known to FDK (after configuration or decoding).
    pub fn stream_info(&self) -> Option<AacStreamInfo> {
        let si = self.raw_stream_info()?;
        let core = si.aacSampleRate.max(0) as u32;
        let sbr = si.flags & ffi::AC_SBR_PRESENT != 0 || si.extAot == ffi::AOT_SBR;
        let ps = si.flags & ffi::AC_PS_PRESENT != 0 || si.aot == ffi::AOT_DRM_MPEG_PS;
        Some(AacStreamInfo {
            sample_rate: si.sampleRate.max(0) as u32,
            frame_size: si.frameSize.max(0) as usize,
            channels: si.numChannels.clamp(0, 8) as u16,
            core_sample_rate: core,
            core_channels: si.aacNumChannels.clamp(0, 8) as u16,
            sbr,
            ps,
            usac: si.aot == ffi::AOT_USAC || si.aot == ffi::AOT_DRM_USAC || si.flags & ffi::AC_USAC != 0,
            output_delay: si.outputDelay,
        })
    }

    /// Output geometry expected from the configuration alone (AAC only).
    fn nominal_geometry(&self) -> Option<(u32, u16, usize)> {
        if self.usac {
            return None;
        }
        let core = self.info.sample_rate()?;
        let (rate, len) = if self.info.sbr { (2 * core, 2 * aac::FRAME_LEN) } else { (core, aac::FRAME_LEN) };
        let ch = if self.info.mode == AudioMode::Mono { 1 } else { 2 };
        Some((rate, ch, len))
    }

    fn clear_input(&mut self) {
        // SAFETY: valid handle; parameter documented to flush the transport buffer.
        unsafe { ffi::aacDecoder_SetParam(self.handle.as_ptr(), ffi::AAC_TPDEC_CLEAR_BUFFER, 1) };
    }

    /// Runs `aacDecoder_DecodeFrame` and converts valid output.
    fn run_decode(&mut self, flags: u32) -> Result<Option<PcmFrame>, CodecError> {
        // SAFETY: `out` is a live buffer of OUT_CAPACITY samples, the size passed.
        let err = unsafe {
            ffi::aacDecoder_DecodeFrame(
                self.handle.as_ptr(),
                self.out.as_mut_ptr(),
                OUT_CAPACITY as i32,
                flags,
            )
        };
        if !ffi::IS_OUTPUT_VALID(err) {
            if err == ffi::AAC_DEC_NOT_ENOUGH_BITS || err == ffi::AAC_DEC_TRANSPORT_SYNC_ERROR {
                self.clear_input();
                return Ok(None);
            }
            self.clear_input();
            return Err(dec_err(err, "decoding"));
        }
        let Some(si) = self.raw_stream_info() else { return Ok(None) };
        let (ch, n) = (si.numChannels, si.frameSize);
        if ch <= 0 || n <= 0 || (ch as usize) * (n as usize) > OUT_CAPACITY {
            return Ok(None);
        }
        let total = ch as usize * n as usize;
        let samples: Vec<f32> = self.out[..total].iter().map(|&s| f32::from(s) / 32768.0).collect();
        let rate = si.sampleRate.max(0) as u32;
        self.last = Some((rate, ch as u16, n as usize));
        Ok(Some(PcmFrame {
            sample_rate: rate,
            channels: ch as u16,
            samples,
            concealed: err != ffi::AAC_DEC_OK || flags & ffi::AACDEC_CONCEAL != 0,
        }))
    }

    fn silence(&self) -> Result<PcmFrame, CodecError> {
        let (rate, ch, n) = self.last.ok_or(CodecError::NothingToConceal)?;
        Ok(PcmFrame { sample_rate: rate, channels: ch, samples: vec![0.0; ch as usize * n], concealed: true })
    }
}

impl DrmAudioDecoder for FdkDrmDecoder {
    /// Decodes one access unit. AAC frames need `crc = Some(aac_crc_bits)` (FDK checks it
    /// and conceals on mismatch, reporting `concealed = true`); for xHE-AAC `crc` is
    /// ignored (xHE-AAC frames carry no per-frame CRC in DRM).
    fn decode(&mut self, frame: &[u8], crc: Option<u8>) -> Result<PcmFrame, CodecError> {
        if frame.is_empty() {
            return self.conceal();
        }
        self.input.clear();
        if !self.usac {
            let c = crc.ok_or_else(|| {
                CodecError::InvalidInput("DRM AAC frames need their aac_crc_bits byte".into())
            })?;
            self.input.push(c); // Dream/FDK convention: CRC byte in front of the frame
        }
        self.input.extend_from_slice(frame);
        let mut ptr = self.input.as_mut_ptr();
        let size = self.input.len() as u32;
        let mut valid = size;
        // SAFETY: `ptr`/`size` describe the live `input` buffer; FDK copies from it and
        // updates `valid`.
        let err = unsafe { ffi::aacDecoder_Fill(self.handle.as_ptr(), &mut ptr, &size, &mut valid) };
        if err != ffi::AAC_DEC_OK || valid != 0 {
            self.clear_input();
            return Err(if err != ffi::AAC_DEC_OK {
                dec_err(err, "filling input")
            } else {
                CodecError::CorruptFrame("decoder input buffer full".into())
            });
        }
        match self.run_decode(0) {
            Ok(Some(pcm)) => Ok(pcm),
            // Nothing decodable: produce concealment instead of a hole in the audio.
            Ok(None) | Err(CodecError::Fdk { .. }) => {
                let mut pcm = self.conceal()?;
                pcm.concealed = true;
                Ok(pcm)
            }
            Err(e) => Err(e),
        }
    }

    fn conceal(&mut self) -> Result<PcmFrame, CodecError> {
        match self.run_decode(ffi::AACDEC_CONCEAL) {
            Ok(Some(pcm)) => Ok(pcm),
            _ => self.silence(),
        }
    }

    fn describe(&self) -> String {
        let si = self.stream_info();
        let (sbr, ps, usac) = match si {
            Some(s) if s.sample_rate > 0 => (s.sbr || self.info.sbr, s.ps, s.usac || self.usac),
            _ => (self.info.sbr, self.info.mode == AudioMode::ParametricStereo, self.usac),
        };
        let khz = |hz: u32| {
            if hz % 1000 == 0 { format!("{}", hz / 1000) } else { format!("{:.1}", hz as f64 / 1000.0) }
        };
        let stereo = match self.info.mode {
            AudioMode::Mono => "mono",
            AudioMode::ParametricStereo => "parametric stereo",
            AudioMode::Stereo => "stereo",
            AudioMode::Reserved => "reserved mode",
        };
        if usac {
            let rate = si.map(|s| s.sample_rate).filter(|&r| r > 0).or(self.info.sample_rate()).unwrap_or(0);
            return format!("xHE-AAC (USAC) {stereo}, {} kHz", khz(rate));
        }
        let core = si
            .map(|s| s.core_sample_rate)
            .filter(|&r| r > 0)
            .or(self.info.sample_rate())
            .unwrap_or(0);
        match (sbr, ps) {
            (true, true) => format!("HE-AAC v2 (SBR+PS) {} kHz core, {} kHz stereo", khz(core), khz(2 * core)),
            (true, false) => format!("HE-AAC (SBR) {stereo}, {} kHz core, {} kHz output", khz(core), khz(2 * core)),
            _ => format!("AAC {stereo}, {} kHz", khz(core)),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------------------

/// AAC flavour produced by [`FdkDrmEncoder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AacProfile {
    /// AAC-LC (no SBR).
    Lc,
    /// HE-AAC v1: AAC + SBR (dual rate: the output rate is twice the core rate).
    HeAac,
    /// HE-AAC v2: mono AAC core + SBR + parametric stereo (stereo input).
    HeAacV2,
}

/// Configuration of [`FdkDrmEncoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdkEncoderConfig {
    /// AAC flavour.
    pub profile: AacProfile,
    /// Stereo (channel pair) core for [`AacProfile::Lc`] / [`AacProfile::HeAac`]; ignored
    /// for HE-AAC v2 (always stereo input, mono core).
    pub stereo: bool,
    /// AAC core sampling rate: 12 000 or 24 000 Hz in DRM30 (ES 201 980 §5.3.1).
    pub core_sample_rate: u32,
    /// Target bit rate of the AAC payload in bit/s. The encoder runs in constant bit rate
    /// mode with the peak rate pinned to this value, so every GA frame FDK produces stays
    /// within `bitrate × 960 / core_sample_rate` bits; DRM re-packing changes the size by
    /// a few bits either way (VCB11/HCR side info added, MPEG-4 element headers and fill
    /// removed) — see [`DrmAacFrame::min_len`] for the actual size.
    pub bitrate: u32,
    /// Frames between SBR headers (`None`: every frame, the most robust choice for a
    /// broadcast that listeners join at arbitrary times).
    pub sbr_header_period: Option<u32>,
}

impl FdkEncoderConfig {
    /// A configuration with the given profile, core rate and bit rate (mono unless
    /// HE-AAC v2).
    pub fn new(profile: AacProfile, core_sample_rate: u32, bitrate: u32) -> Self {
        Self { profile, stereo: false, core_sample_rate, bitrate, sbr_header_period: None }
    }

    /// Whether SBR is used.
    pub fn sbr(&self) -> bool {
        self.profile != AacProfile::Lc
    }

    /// Sampling rate of the PCM passed to [`FdkDrmEncoder::encode`].
    pub fn input_sample_rate(&self) -> u32 {
        if self.sbr() { 2 * self.core_sample_rate } else { self.core_sample_rate }
    }

    /// Channels of the PCM passed to [`FdkDrmEncoder::encode`].
    pub fn input_channels(&self) -> usize {
        if self.profile == AacProfile::HeAacV2 || self.stereo { 2 } else { 1 }
    }

    /// Input samples per channel per frame (960, or 1920 with SBR).
    pub fn frame_len(&self) -> usize {
        if self.sbr() { 2 * aac::FRAME_LEN } else { aac::FRAME_LEN }
    }

    /// DRM audio mode signalled in the SDC.
    pub fn audio_mode(&self) -> AudioMode {
        match (self.profile, self.stereo) {
            (AacProfile::HeAacV2, _) => AudioMode::ParametricStereo,
            (_, true) => AudioMode::Stereo,
            _ => AudioMode::Mono,
        }
    }
}

/// One DRM AAC access unit produced by [`FdkDrmEncoder`], plus its `aac_crc_bits`.
///
/// The frame consists of the core AAC bits followed — when SBR is used — by the SBR
/// payload stored **bit-reversed at the very end of the frame** (DRM convention, FDK's
/// `sbrDecoder_Parse` reads it backwards from the last byte). Any padding therefore has
/// to go *between* the two parts; use [`DrmAacFrame::to_bytes_padded`] when the super-frame
/// packer assigns a frame more bytes than [`DrmAacFrame::min_len`] (for example the last
/// frame of an audio super frame, whose length is implicit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmAacFrame {
    crc: u8,
    core: BitBuf,
    sbr: Option<BitBuf>,
}

impl DrmAacFrame {
    /// The frame's `aac_crc_bits` byte (ES 201 980 §5.3.1.2), transmitted in the audio super
    /// frame separately from the frame bytes.
    pub fn crc(&self) -> u8 {
        self.crc
    }

    /// Minimum length in bytes.
    pub fn min_len(&self) -> usize {
        (self.core.len() + self.sbr.as_ref().map_or(0, |s| s.len())).div_ceil(8)
    }

    /// Whether the frame carries SBR data.
    pub fn has_sbr(&self) -> bool {
        self.sbr.is_some()
    }

    /// The frame at its minimum length.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.pack(self.min_len())
    }

    /// The frame padded to `len >= min_len()` bytes (zero bits inserted between the core
    /// and the SBR tail).
    pub fn to_bytes_padded(&self, len: usize) -> Result<Vec<u8>, CodecError> {
        if len < self.min_len() {
            return Err(CodecError::InvalidInput(format!(
                "frame needs {} bytes, {len} requested",
                self.min_len()
            )));
        }
        Ok(self.pack(len))
    }

    fn pack(&self, len: usize) -> Vec<u8> {
        let mut out = BitBuf::zeros(len * 8);
        for i in 0..self.core.len() {
            out.set(i, self.core.get(i));
        }
        if let Some(sbr) = &self.sbr {
            let last = len * 8 - 1;
            for i in 0..sbr.len() {
                out.set(last - i, sbr.get(i));
            }
        }
        out.as_bytes().to_vec()
    }
}

/// DRM AAC encoder: FDK-AAC (patched for 960-sample frames and DRM SBR payloads) plus the
/// MPEG-4 → DRM access-unit converter of this crate.
///
/// FDK has no DRM transport and no HCR writer (and `aacEncoder_SetParam(AACENC_TRANSMUX,
/// TT_DRM)`, which Dream tries, is rejected), so each raw MPEG-4 access unit is parsed down
/// to its quantised spectrum and re-serialised in DRM syntax: VCB11 section data, HCR
/// spectral data, `aac_crc_bits` computed with the DRM CRC-8 over the same region FDK's
/// decoder checks, and the DRM SBR payload (whose own 8-bit CRC FDK's SBR encoder
/// computes) moved to the end of the frame. The conversion is lossless.
pub struct FdkDrmEncoder {
    handle: NonNull<ffi::AACENCODER>,
    config: FdkEncoderConfig,
    table: aac::SfbTable,
    input: Vec<i16>,
    output: Vec<u8>,
    delay: usize,
}

// SAFETY: as for the decoder — exclusively owned instance without thread affinity.
unsafe impl Send for FdkDrmEncoder {}

impl Drop for FdkDrmEncoder {
    fn drop(&mut self) {
        let mut h = self.handle.as_ptr();
        // SAFETY: the handle came from aacEncOpen and is closed exactly once here.
        unsafe { ffi::aacEncClose(&mut h) };
    }
}

impl FdkDrmEncoder {
    /// Creates and initialises an encoder.
    pub fn new(config: FdkEncoderConfig) -> Result<Self, CodecError> {
        let table = sfb_table_960(config.core_sample_rate).ok_or_else(|| {
            CodecError::InvalidConfig(format!("unsupported AAC core rate {} Hz", config.core_sample_rate))
        })?;
        if !matches!(config.core_sample_rate, 12_000 | 24_000 | 48_000) {
            return Err(CodecError::InvalidConfig(format!(
                "DRM AAC core rate must be 12, 24 or 48 kHz, not {} Hz",
                config.core_sample_rate
            )));
        }
        let channels = config.input_channels() as u32;
        let mut raw: ffi::HANDLE_AACENCODER = std::ptr::null_mut();
        // SAFETY: valid out-pointer; 0 = allocate all encoder modules.
        let err = unsafe { ffi::aacEncOpen(&mut raw, 0, channels) };
        let handle = match NonNull::new(raw) {
            Some(h) if err == ffi::AACENC_OK => h,
            _ => return Err(enc_err(err, "aacEncOpen")),
        };
        let mut enc = FdkDrmEncoder {
            handle,
            table,
            input: vec![0; config.frame_len() * config.input_channels()],
            output: Vec::new(),
            delay: 0,
            config,
        };
        let c = &enc.config;
        let aot = match c.profile {
            AacProfile::Lc => ffi::AOT_AAC_LC,
            AacProfile::HeAac => ffi::AOT_SBR,
            AacProfile::HeAacV2 => ffi::AOT_PS,
        };
        let mode = if c.input_channels() == 2 { ffi::MODE_2 } else { ffi::MODE_1 };
        let mut params: Vec<(ffi::AACENC_PARAM, u32, &'static str)> = vec![
            (ffi::AACENC_AOT, aot as u32, "AOT"),
            (ffi::AACENC_SAMPLERATE, c.input_sample_rate(), "sample rate"),
            (ffi::AACENC_CHANNELMODE, mode as u32, "channel mode"),
            (ffi::AACENC_GRANULE_LENGTH, aac::FRAME_LEN as u32, "960-sample granule"),
            (ffi::AACENC_TRANSMUX, ffi::TT_MP4_RAW as u32, "raw transport"),
            (ffi::AACENC_BITRATEMODE, 0, "CBR"),
            (ffi::AACENC_BITRATE, c.bitrate, "bit rate"),
            (ffi::AACENC_PEAK_BITRATE, c.bitrate, "peak bit rate"),
            (ffi::AACENC_AFTERBURNER, 1, "afterburner"),
        ];
        if c.sbr() {
            params.push((ffi::AACENC_DECDRM_DRM_SBR, 1, "DRM SBR syntax"));
            params.push((ffi::AACENC_HEADER_PERIOD, c.sbr_header_period.unwrap_or(1).clamp(1, 255), "SBR header period"));
        }
        for (param, value, what) in params {
            // SAFETY: valid handle; integer parameters.
            let err = unsafe { ffi::aacEncoder_SetParam(enc.handle.as_ptr(), param, value) };
            if err != ffi::AACENC_OK {
                return Err(enc_err(err, what));
            }
        }
        // SAFETY: a call with NULL buffers initialises the encoder (aacenc_lib.h).
        let err = unsafe {
            ffi::aacEncEncode(
                enc.handle.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        if err != ffi::AACENC_OK {
            return Err(enc_err(err, "initialising (check bit rate / sample rate)"));
        }
        // SAFETY: zeroed POD out-struct filled by aacEncInfo.
        let mut info: ffi::AACENC_InfoStruct = unsafe { std::mem::zeroed() };
        // SAFETY: valid handle and out-pointer.
        let err = unsafe { ffi::aacEncInfo(enc.handle.as_ptr(), &mut info) };
        if err != ffi::AACENC_OK {
            return Err(enc_err(err, "aacEncInfo"));
        }
        if info.frameLength as usize != enc.config.frame_len() {
            return Err(CodecError::InvalidConfig(format!(
                "FDK frame length {} (expected {})",
                info.frameLength,
                enc.config.frame_len()
            )));
        }
        enc.delay = info.nDelay as usize;
        enc.output = vec![0; (info.maxOutBufBytes as usize).max(8192)];
        Ok(enc)
    }

    /// The configuration.
    pub fn config(&self) -> &FdkEncoderConfig {
        &self.config
    }

    /// Encoder delay in input samples per channel.
    pub fn delay(&self) -> usize {
        self.delay
    }

    /// The SDC type-9 audio information a receiver needs to decode this stream (port of
    /// `CAudioParam::getType9Bytes` semantics; call `.to_type9_bytes()` on it).
    pub fn audio_info(&self) -> AudioInfo {
        AudioInfo::aac(self.config.core_sample_rate, self.config.sbr(), self.config.audio_mode())
            .expect("validated in new()")
    }

    /// Encodes one frame of interleaved PCM (`frame_len() × input_channels()` samples at
    /// `input_sample_rate()`, nominal range ±1). Returns `None` while the encoder's delay
    /// line is filling.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Option<DrmAacFrame>, CodecError> {
        if pcm.len() != self.input.len() {
            return Err(CodecError::InvalidInput(format!(
                "expected {} samples per frame, got {}",
                self.input.len(),
                pcm.len()
            )));
        }
        for (d, &s) in self.input.iter_mut().zip(pcm) {
            *d = (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
        }
        let mut in_ptr = self.input.as_mut_ptr().cast::<std::ffi::c_void>();
        let mut in_id = ffi::IN_AUDIO_DATA;
        let mut in_size = (self.input.len() * 2) as i32;
        let mut in_el = 2i32;
        let in_desc = ffi::AACENC_BufDesc {
            numBufs: 1,
            bufs: &mut in_ptr,
            bufferIdentifiers: &mut in_id,
            bufSizes: &mut in_size,
            bufElSizes: &mut in_el,
        };
        let mut out_ptr = self.output.as_mut_ptr().cast::<std::ffi::c_void>();
        let mut out_id = ffi::OUT_BITSTREAM_DATA;
        let mut out_size = self.output.len() as i32;
        let mut out_el = 1i32;
        let out_desc = ffi::AACENC_BufDesc {
            numBufs: 1,
            bufs: &mut out_ptr,
            bufferIdentifiers: &mut out_id,
            bufSizes: &mut out_size,
            bufElSizes: &mut out_el,
        };
        let in_args = ffi::AACENC_InArgs { numInSamples: self.input.len() as i32, numAncBytes: 0 };
        let mut out_args = ffi::AACENC_OutArgs::default();
        // SAFETY: the descriptors point at live locals and buffers whose sizes are given in
        // bytes as the API requires; FDK does not retain the pointers.
        let err = unsafe {
            ffi::aacEncEncode(self.handle.as_ptr(), &in_desc, &out_desc, &in_args, &mut out_args)
        };
        if err != ffi::AACENC_OK {
            return Err(enc_err(err, "encoding"));
        }
        if out_args.numInSamples != self.input.len() as i32 {
            return Err(CodecError::Repack(format!(
                "FDK consumed {} of {} samples",
                out_args.numInSamples,
                self.input.len()
            )));
        }
        let n = out_args.numOutBytes.max(0) as usize;
        if n == 0 {
            return Ok(None);
        }
        let Some(au) = aac::ga::parse_raw_data_block(&self.output[..n], &self.table)? else {
            return Ok(None);
        };
        let drm = aac::drm::repack(&au)?;
        Ok(Some(DrmAacFrame { crc: drm.crc, core: drm.core, sbr: drm.sbr }))
    }
}
