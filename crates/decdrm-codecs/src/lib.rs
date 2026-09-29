//! Safe audio codec wrappers for DecDRM: FDK-AAC for DRM AAC / HE-AAC v1/v2 / xHE-AAC and
//! libopus for Dream's Opus extension.
//!
//! # Receiving
//!
//! The receiver (`decdrm-core`) cuts audio super frames into codec frames and reads the
//! service's SDC entity type 9 (*audio information*). With those, decoding is:
//!
//! ```no_run
//! use decdrm_codecs::{open_decoder, AudioInfo};
//! # fn run(sdc_type9: &[u8], frames: &[(Vec<u8>, u8)]) -> Result<(), decdrm_codecs::CodecError> {
//! let info = AudioInfo::from_type9_bytes(sdc_type9)?;
//! let coding = info.drm_audio_coding().expect("CELP/HVXC are not supported");
//! let mut dec = open_decoder(coding, sdc_type9)?;
//! println!("{}", dec.describe());
//! for (frame, crc) in frames {
//!     let pcm = dec.decode(frame, Some(*crc))?; // interleaved f32
//!     # let _ = pcm;
//! }
//! # Ok(()) }
//! ```
//!
//! **SDC bytes.** `sdc_type9` is the type-9 entity body *without* the leading short Id /
//! stream Id nibble — two bytes (audio coding … rfa) followed, for xHE-AAC, by the xHE-AAC
//! config bytes. This is what Dream's `CAudioParam::getType9Bytes()` produces and what
//! FDK's `aacDecoder_ConfigRaw()` expects for `TT_DRM`. [`AudioInfo::from_entity_body`]
//! converts a complete entity body (with the Ids).
//!
//! **CRC bytes.** For AAC pass each frame's `aac_crc_bits` byte from the audio super frame
//! as `crc`; the decoder prepends it to the frame (as Dream does) and FDK verifies it over
//! the frame's error-sensitive part. For Opus pass the byte found in the same position;
//! it is checked with Dream's CRC ([`crc::dream_opus_crc`]). For xHE-AAC the argument is
//! ignored (xHE-AAC frames have no per-frame CRC; the super-frame header has its own).
//!
//! **Errors versus concealment.** A frame that fails its CRC or does not parse still
//! yields audio: FDK's error concealment or Opus FEC/PLC output, flagged with
//! [`PcmFrame::concealed`]. `Err` is returned only when no audio can be produced at all.
//!
//! # Transmitting
//!
//! [`FdkDrmEncoder`] produces DRM AAC access units (AAC, HE-AAC, HE-AAC v2) together with
//! their `aac_crc_bits`, and [`OpusDrmEncoder`] Dream-compatible Opus frames. Both report
//! the [`AudioInfo`] to put into SDC entity 9, so a loopback test can open the decoder
//! exactly as a receiver would.
//!
//! # Notes on the Rust idioms used
//!
//! * The codec handles are C pointers owned by small wrapper structs. Their `Drop`
//!   implementations free the C state automatically when the wrapper goes out of scope —
//!   there is no `close()` to forget, and no way to use a closed handle.
//! * Decoders are handed around as `Box<dyn DrmAudioDecoder>`: a heap-allocated value of
//!   some type implementing the trait, chosen at run time (Rust's version of a virtual
//!   base class pointer). The trait requires `Send`, so a decoder can be moved to the
//!   audio thread. The wrappers are *not* `Sync` (not shareable between threads by
//!   reference), because the C libraries do no locking; see [`fdk`](crate::fdk) for the
//!   details.
//! * `Result<T, CodecError>` is returned instead of throwing; `?` propagates errors.

mod aac;
mod bits;
pub mod crc;
mod fdk;
mod opus;
pub mod sdc;

pub use fdk::{
    AacProfile, AacStreamInfo, DrmAacFrame, FdkDrmDecoder, FdkDrmEncoder, FdkEncoderConfig,
    FdkLibInfo, fdk_lib_info,
};
pub use opus::{
    OPUS_FRAME_LEN, OPUS_MAX_PACKET, OPUS_SAMPLE_RATE, OpusApplication, OpusBandwidth,
    OpusDrmDecoder, OpusDrmEncoder, OpusDrmFrame, OpusEncoderConfig, OpusSignal, opus_version,
};
pub use sdc::{AudioCodingField, AudioInfo, AudioMode, OpusSignalling};

/// A block of decoded audio.
///
/// Compared with the originally sketched interface this carries one extra field,
/// [`concealed`](Self::concealed): FDK and libopus return *usable* audio for damaged frames
/// (error concealment, Opus FEC/PLC), and the receiver needs to know when that happened
/// for its audio-quality statistics without losing the samples.
#[derive(Debug, Clone, PartialEq)]
pub struct PcmFrame {
    /// Sampling rate in Hz.
    pub sample_rate: u32,
    /// Number of interleaved channels (1 or 2).
    pub channels: u16,
    /// Interleaved samples, nominal range [-1, 1].
    pub samples: Vec<f32>,
    /// The samples are concealment (or FEC reconstruction) rather than a clean decode.
    pub concealed: bool,
}

impl PcmFrame {
    /// Samples per channel.
    pub fn frames(&self) -> usize {
        if self.channels == 0 { 0 } else { self.samples.len() / usize::from(self.channels) }
    }

    /// Duration in seconds.
    pub fn duration(&self) -> f64 {
        if self.sample_rate == 0 { 0.0 } else { self.frames() as f64 / f64::from(self.sample_rate) }
    }
}

/// The audio codings DecDRM can decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DrmAudioCoding {
    /// AAC, HE-AAC (SBR), HE-AAC v2 (SBR + PS) — ES 201 980 §5.3.1.
    Aac,
    /// xHE-AAC (MPEG-D USAC) — ES 201 980 §5.3.1 (V4).
    XheAac,
    /// Opus — Dream's non-standard extension.
    Opus,
}

/// A decoder for one DRM audio service.
pub trait DrmAudioDecoder: Send {
    /// Decodes one codec access unit cut from a DRM audio super frame.
    ///
    /// For AAC `crc` is that frame's `aac_crc_bits` byte (required); for Opus the byte
    /// in the same position (optional, `None` skips the check); for xHE-AAC it is ignored.
    /// An empty `frame` is treated as lost and concealed.
    fn decode(&mut self, frame: &[u8], crc: Option<u8>) -> Result<PcmFrame, CodecError>;

    /// Concealment output for a lost frame (FDK `AACDEC_CONCEAL`, Opus PLC), of the
    /// duration of the last decoded frame.
    fn conceal(&mut self) -> Result<PcmFrame, CodecError>;

    /// A short description such as `"HE-AAC v2 (SBR+PS) 12 kHz core, 24 kHz stereo"`.
    fn describe(&self) -> String;
}

/// Opens the decoder for `coding`, configured from the SDC type-9 bytes (see the crate
/// docs). For Opus the bytes are not needed (Dream's decoder ignores them) and may be
/// empty.
pub fn open_decoder(
    coding: DrmAudioCoding,
    sdc_type9: &[u8],
) -> Result<Box<dyn DrmAudioDecoder>, CodecError> {
    match coding {
        DrmAudioCoding::Opus => Ok(Box::new(OpusDrmDecoder::new()?)),
        DrmAudioCoding::Aac | DrmAudioCoding::XheAac => {
            let info = AudioInfo::from_type9_bytes(sdc_type9)?;
            let signalled = info.drm_audio_coding();
            if signalled != Some(coding) {
                return Err(CodecError::InvalidConfig(format!(
                    "SDC type 9 signals {signalled:?}, decoder for {coding:?} requested"
                )));
            }
            Ok(Box::new(FdkDrmDecoder::new(&info)?))
        }
    }
}

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodecError {
    /// The SDC audio information or an encoder configuration is invalid.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// The service uses a coding DecDRM cannot handle.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// An FDK-AAC call failed.
    #[error("FDK-AAC error {code:#06x} ({name}) while {context}")]
    Fdk {
        /// `AAC_DECODER_ERROR` / `AACENC_ERROR` value.
        code: i32,
        /// Description of the code.
        name: &'static str,
        /// What the wrapper was doing.
        context: &'static str,
    },
    /// A libopus call failed.
    #[error("Opus error {code} ({message}) in {context}")]
    Opus {
        /// libopus error code.
        code: i32,
        /// `opus_strerror` text.
        message: String,
        /// The failing call.
        context: &'static str,
    },
    /// A frame was damaged beyond concealment.
    #[error("corrupt frame: {0}")]
    CorruptFrame(String),
    /// Concealment was requested before any audio was decoded.
    #[error("nothing to conceal yet")]
    NothingToConceal,
    /// Wrong input to an encoder or decoder call.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// An AAC bitstream read failed (internal MPEG-4 → DRM conversion).
    #[error("AAC bitstream error: {0}")]
    Bitstream(&'static str),
    /// FDK's output could not be converted to DRM syntax.
    #[error("DRM re-packing failed: {0}")]
    Repack(String),
}
