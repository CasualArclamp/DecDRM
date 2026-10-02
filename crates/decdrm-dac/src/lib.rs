//! # decdrm-dac — DAC as DecDRM's neural audio codec
//!
//! [DAC](https://arxiv.org/abs/2306.06546), the Descript Audio Codec ("High-Fidelity
//! Audio Compression with Improved RVQGAN", MIT code and weights), codes audio as frames
//! of residual-vector-quantiser codes. Its 24 kHz model makes 75 frames per second of
//! up to 32 codes of 10 bits; DecDRM sends 2, 4, 8, 16 or 32 of them per frame, i.e.
//! 1.5, 3, 6, 12 or 24 kbit/s. This crate carries it over DRM30 as a **DecDRM-only
//! extension**: standard receivers see a reserved audio coding and ignore the service.
//!
//! DAC replaced EnCodec (Meta's codec, which DecDRM 0.4.6 and earlier used) after a
//! comparison on broadcast audio and speech (2026-10-02; `docs/DESIGN.md`): at every
//! bit rate DAC was closer to the original by every measure used, DAC at 3 kbit/s about
//! as close as EnCodec at 12, and with 20 % of its frames lost still as close as
//! EnCodec without losses. Its price is computation: see *Speed*. EnCodec services of
//! older versions are recognised and named, not decoded.
//!
//! ## What is where
//!
//! Always available (plain Rust, no neural network):
//!
//! * [`config`] — constants, [`Bandwidth`] and [`DacConfig`], the codec configuration
//!   signalled in SDC entity type 9;
//! * [`framing`] — the audio super frame: codes, CRCs, repetition ([`FrameLayout`]);
//! * [`plan`] — the bandwidth and framing that fit a stream ([`choose_config`]);
//! * [`weights`] — where the model weights live, and downloading them.
//!
//! With the `dac` cargo feature (pulls in candle, CPU inference):
//!
//! * [`model`] — the streaming DAC network ([`DacModel`]);
//! * [`DacEncoder`] / [`DacDrmEncoder`] — PCM → codes → super frames;
//! * [`DacDecoder`] — super frames → PCM, implementing `decdrm_codecs::DrmAudioDecoder`
//!   like the AAC/Opus decoders ([`open_decoder`]).
//!
//! ## Signalling (SDC entity type 9)
//!
//! ```text
//! byte 0   10 0 00 011   audio coding 10 (reserved in ES 201 980 V4), SBR/mode rfa = 0,
//!                        sampling rate code 011 ("24 kHz")
//! byte 1   t 0 00000 0   text flag, enhancement 0, coder field and rfa 0
//! bytes 2… 00 44 41 43 31 pp   codec specific config: NUL, "DAC1", parameter byte
//!          pp = bandwidth tier (3 bits: 1.5/3/6/12/24 kbit/s)
//!             | CRC group size code (3 bits: 1, 2, 3, 5, 6, 10, 15, 30 frames)
//!             | repeated layers (2 bits: 0-3)
//! ```
//!
//! ES 201 980 defines no codec config for audio coding 10, and its SBR, mode, rate and
//! coder fields as rfa. Two details keep Dream (the reference software receiver) happy:
//! it rejects coding-10 entities whose rate code is not 001/011/101 — and then stops
//! parsing the rest of the SDC block — so the rate code is 011; and it does not skip
//! the config bytes by the entity length but reads on from them as the next entity
//! header, so the config starts with a zero byte, which reads as the end-of-entities
//! marker. The transmitter sends this entity last in its SDC block, so nothing after it
//! is lost to such receivers. The '1' of "DAC1" is the framing format version. (The
//! EnCodec services of DecDRM 0.4.6 and earlier sent "ENC1" with the same layout.)
//!
//! ## Framing
//!
//! One 400 ms audio super frame carries 30 DAC frames. The codebooks form *layers*:
//! layer 0 = codebooks 0–1 (1.5 kbit/s), layer *l* = the codebooks tier *l* adds
//! (2–3, 4–7, 8–15, 16–31). The codes are sent layer by layer; each layer is split into
//! groups of *G* frames, and every (layer, group) *region* ends with a CRC-8 (the DRM
//! polynomial). Then come the first *R* layers again (same bits), then zero padding up
//! to the stream length. See [`framing`] for the exact bit order.
//!
//! Why this shape:
//!
//! * **Per group, not per super frame.** Errors after DRM's Viterbi decoder come in
//!   short bursts. One CRC per super frame would throw away 400 ms for every burst;
//!   per-frame CRCs would cost 30 bytes per layer. Groups of 3 frames (40 ms, the
//!   length of an AAC frame at a 24 kHz core) are the default; a burst rarely spans more
//!   than one region because regions are contiguous in the bit stream.
//! * **Per layer.** Codebook *k* only refines what codebooks 0…k−1 left, so the layers
//!   differ in importance: a wrong base code (layer 0) produces a wrong sound, a wrong
//!   fine code a small error. Separate CRCs let a receiver treat them differently,
//!   repair each repeated layer from whichever copy is intact, and count errors by
//!   layer.
//! * **Spare bytes buy redundancy.** The multiplex gives the audio stream all the
//!   capacity the data services leave, while the bit rates come in factors of two, so
//!   there is usually room left. It is spent on sending the most important layers twice
//!   (a region is lost only if both copies fail: probability *p* becomes about *p*²),
//!   then on finer CRC groups; see [`plan`].
//!
//! ## Errors and concealment
//!
//! Default receiver policy ([`CrcPolicy::Adaptive`], [`Concealment::Interpolate`]): the
//! codes of regions that failed their CRC (in every copy) are decoded as received,
//! unless at least 90 % of the super frame's regions failed — garbage, e.g. while the
//! signal is being lost. In such a super frame a frame whose base layer failed is lost
//! and concealed in the latent domain — DAC's decoder input, so the network itself
//! produces a smooth transition — by interpolating between the neighbouring good frames
//! (gaps up to 120 ms inside a super frame), otherwise by repeating the last latent for
//! 40 ms and fading out over 80 ms, with a 13 ms fade-in when good frames return.
//!
//! Measured with `examples/dac_errors.rs` (12 s of tone, chirp and speech-like bursts at
//! 6 kbit/s, 40 ms groups, no repetition, 3 seeds; log-spectral distance / segmental
//! noise-to-reference ratio against the error-free decoding, dB, lower is better):
//!
//! | errors | **adaptive** | ignore CRCs | strict + interpolate | trust enh. + interpolate | + repeat | + mute |
//! |---|---|---|---|---|---|---|
//! | bursts 3·10⁻⁴/bit | **0.45 / −36.0** | 0.45 / −36.0 | 0.63 / −33.6 | 0.46 / −35.4 | 0.50 / −35.3 | 0.62 / −35.3 |
//! | bursts 10⁻³/bit | **1.28 / −29.4** | 1.28 / −29.4 | 2.30 / −22.9 | 1.72 / −27.5 | 1.93 / −27.3 | 2.17 / −27.4 |
//! | bursts 3·10⁻³/bit | **3.71 / −18.3** | 3.71 / −18.3 | 4.96 / −9.6 | 4.15 / −14.7 | 4.68 / −14.1 | 5.65 / −14.3 |
//! | 10 % garbage super frames | **2.66 / −35.4** | 2.51 / −35.1 | 2.66 / −35.4 | 2.66 / −35.4 | 2.66 / −35.4 | 2.90 / −35.5 |
//! | 30 % garbage super frames | **5.61 / −28.1** | 6.86 / −27.2 | 5.61 / −28.1 | 5.61 / −28.1 | 5.61 / −28.1 | 5.97 / −28.2 |
//!
//! A burst leaves one or two wrong codes in a region; with DAC they cost less than
//! concealing the region's frames, unlike with EnCodec, where trusting the enhancement
//! layers and concealing a failed base layer was best. Garbage super frames are still
//! better concealed. With the base layer sent twice (6 and 3 kbit/s) few base failures
//! survive the repair: up to 3·10⁻³/bit adaptive and trust enh. + interpolate come
//! within 0.03 dB of each other in LSD, adaptive with the better NRR (6 kbit/s at
//! 3·10⁻³/bit: 1.94 / −22.4 against 1.93 / −21.5), and they conceal garbage super
//! frames alike. Only at 10⁻²/bit, far beyond what audio survives, does trust enh. give
//! the lower LSD (6 kbit/s: 7.18 / −6.6 against 7.64 / −9.3).
//!
//! ## Delay and streaming
//!
//! DAC's convolutions are centred, so [`model`] streams them with each layer's
//! look-ahead: the decoder's output lags by 131 ms ([`model::DECODER_LAG`]), the
//! encoder starts each stream with 107 ms of silence ([`model::ENCODER_LEAD_IN`]).
//! Chunked processing is exact: 400 ms at a time gives what one run over the whole
//! signal gives. The receiver loads only the decoder half, the transmitter only the
//! encoder half.
//!
//! ## Speed
//!
//! CPU inference with candle, 400 ms per call (`examples/dac_rtf.rs`, release build,
//! Ryzen 7 9800X3D): real-time factor 0.24 for decoding and 0.15 for encoding on all 16
//! threads, 0.83 and 0.41 on one core — about twelve times EnCodec's work, as the
//! network is that much larger (a 1536-channel decoder). A receiver needs a reasonably
//! modern CPU; on a slow one the audio stutters.
//!
//! ## Model weights
//!
//! `descript/dac_24khz` (`model.safetensors`, 299 MB) is downloaded once with
//! `decdrm models download dac`; see [`weights`] for the directory rules
//! (`$DECDRM_MODELS`, else `models` next to the executable).

#![forbid(unsafe_code)]
// The crate docs describe the `dac` build; without the feature most items they link to
// are compiled out.
#![cfg_attr(not(feature = "dac"), allow(rustdoc::broken_intra_doc_links))]

pub mod config;
mod crc;
mod error;
pub mod framing;
pub mod plan;
mod sha256;
pub mod weights;

#[cfg(feature = "dac")]
mod decoder;
#[cfg(feature = "dac")]
mod encoder;
#[cfg(feature = "dac")]
pub mod model;

pub use config::{
    Bandwidth, CODE_BITS, CODEBOOK_SIZE, ConfigError, DacConfig, FRAME_RATE, FRAME_SAMPLES, FRAMES_PER_SUPER_FRAME,
    LATENT_DIM, MAX_CODEBOOKS, SAMPLE_RATE, SUPER_FRAME_SAMPLES,
};
pub use crc::Crc8;
pub use error::DacError;
pub use framing::{Codes, FrameLayout, FramingError, Unpacked};
pub use plan::{PlanError, choose_config};
pub use weights::{WeightsNotFound, find_weights};

#[cfg(feature = "dac")]
pub use decoder::{
    Concealment, CrcPolicy, DacDecoder, DecodedSuperFrame, DecoderStats, FADE_FRAMES, GARBAGE_FRACTION, HOLD_FRAMES,
    MAX_INTERPOLATION_FRAMES, open_decoder,
};
#[cfg(feature = "dac")]
pub use encoder::{DacDrmEncoder, DacEncoder};
#[cfg(feature = "dac")]
pub use model::{DacModel, ModelParts};

/// Whether this build contains the codec itself (the `dac` feature); without it, DAC
/// services can be planned, framed and described but not encoded or decoded.
pub const BUILT_IN: bool = cfg!(feature = "dac");
