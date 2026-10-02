//! # decdrm-encodec — EnCodec as an experimental DecDRM audio codec
//!
//! [EnCodec](https://arxiv.org/abs/2210.13438) is Meta's neural audio codec. Its 24 kHz
//! model codes mono audio as 75 frames per second of residual-vector-quantiser codes:
//! 2, 4, 8, 16 or 32 codebooks of 1024 entries (10 bits) per frame, i.e. 1.5, 3, 6, 12
//! or 24 kbit/s. This crate carries it over DRM30 as a **DecDRM-only extension**
//! (milestone M9): standard receivers see a reserved audio coding and ignore the
//! service.
//!
//! ## What is where
//!
//! Always available (plain Rust, no neural network):
//!
//! * [`config`] — constants, [`Bandwidth`] and [`EncodecConfig`], the codec
//!   configuration signalled in SDC entity type 9;
//! * [`framing`] — the audio super frame: codes, CRCs, repetition ([`FrameLayout`]);
//! * [`plan`] — the bandwidth and framing that fit a stream ([`choose_config`]);
//! * [`weights`] — where the model weights live, and downloading them.
//!
//! With the `encodec` cargo feature (pulls in candle, CPU inference):
//!
//! * [`model`] — the streaming EnCodec network ([`EncodecModel`]);
//! * [`EncodecEncoder`] / [`EncodecDrmEncoder`] — PCM → codes → super frames;
//! * [`EncodecDecoder`] — super frames → PCM, implementing
//!   `decdrm_codecs::DrmAudioDecoder` like the AAC/Opus decoders ([`open_decoder`]).
//!
//! ## Signalling (SDC entity type 9)
//!
//! ```text
//! byte 0   10 0 00 011   audio coding 10 (reserved in ES 201 980 V4), SBR/mode rfa = 0,
//!                        sampling rate code 011 ("24 kHz")
//! byte 1   t 0 00000 0   text flag, enhancement 0, coder field and rfa 0
//! bytes 2… 00 45 4E 43 31 pp   codec specific config: NUL, "ENC1", parameter byte
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
//! is lost to such receivers. The '1' of "ENC1" is the framing format version.
//!
//! ## Framing
//!
//! One 400 ms audio super frame carries 30 EnCodec frames. The codebooks form *layers*:
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
//!   capacity the data services leave, while EnCodec's bit rates come in factors of
//!   two, so there is usually room left. It is spent on sending the most important
//!   layers twice (a region is lost only if both copies fail: probability *p* becomes
//!   about *p*²), then on finer CRC groups; see [`plan`].
//!
//! ## Errors and concealment
//!
//! Default receiver policy ([`CrcPolicy::TrustEnhancement`], [`Concealment::Interpolate`]):
//! a frame whose base layer failed (in every copy) is lost and concealed in the latent
//! domain — EnCodec's decoder input, so the network itself produces a smooth
//! transition — by interpolating between the neighbouring good frames (gaps up to
//! 120 ms inside a super frame), otherwise by repeating the last latent for 40 ms and
//! fading out over 80 ms, with a 13 ms fade-in when good frames return. Failed
//! enhancement layers are decoded as received.
//!
//! Measured with `examples/encodec_errors.rs` (12 s of tone, chirp and speech-like
//! bursts at 6 kbit/s, 40 ms groups; log-spectral distance / segmental
//! noise-to-reference ratio against the error-free decoding, lower is better):
//!
//! | errors | ignore CRCs | strict: drop failed layers | **trust enh. + interpolate** | + repeat | + mute |
//! |---|---|---|---|---|---|
//! | bursts 3·10⁻⁴/bit | 0.57 / −33.8 | 0.66 / −33.7 | **0.56 / −34.4** | 0.60 / −34.0 | 0.83 / −34.1 |
//! | bursts 10⁻³/bit | 1.30 / −27.2 | 1.75 / −23.1 | **1.41 / −26.7** | 1.59 / −26.3 | 2.16 / −26.5 |
//! | bursts 3·10⁻³/bit | 3.53 / −16.7 | 3.70 / −10.4 | **3.21 / −14.9** | 3.61 / −14.0 | 4.97 / −14.3 |
//! | 10 % garbage super frames | 2.39 / −30.4 | 2.97 / −32.7 | **2.97 / −32.7** | 2.97 / −32.7 | 3.19 / −32.7 |
//! | 30 % garbage super frames | 7.13 / −18.7 | 6.25 / −22.5 | **6.25 / −22.5** | 6.25 / −22.5 | 6.59 / −22.6 |
//!
//! Dropping failed enhancement layers is the worst choice under bursts (a burst spoils
//! one or two codes of a region; dropping discards all of them and every layer above);
//! interpolation beats repetition and muting; ignoring the CRCs does about as well on
//! sparse bursts but turns corrupt super frames into loud babble (worse noise ratio;
//! the spectral distance favours babble over silence). Repeating the base layer halves
//! the distortion (bursts 10⁻³: 0.75 dB / −31.6 dB).
//!
//! Over a real DRM channel (station → white noise → DecDRM's receiver; mode B, 10 kHz,
//! 64-QAM, protection 1, long interleaving; the ignored test `encodec_under_noise` of
//! `decdrm-station`), EnCodec at 12 kbit/s with codebooks 0–7 sent twice against HE-AAC
//! using the whole 20 kbit/s stream:
//!
//! | SNR | EnCodec frames concealed | HE-AAC frames concealed |
//! |---|---|---|
//! | 15.5 dB | 0 % (5 regions failed, all repaired) | 0.4 % |
//! | 15.0 dB | 2.1 % (166 of 240 failed regions repaired) | 26.5 % |
//! | 14.5 dB | 46 % | 98 % |
//!
//! ## Speed
//!
//! CPU inference with candle, 400 ms per call (`examples/encodec_rtf.rs`, release build,
//! 16-thread desktop CPU): real-time factor about 0.06 for encoding and for decoding at
//! every bandwidth (25–30 ms per super frame), 0.07 on a single core — the network is
//! mostly sequential (LSTM steps, many small layers), so more cores help little. Loading
//! the weights takes 0.05–0.07 s.
//!
//! ## Streaming inference
//!
//! EnCodec 24 kHz is causal, so [`model`] runs its layers chunk by chunk with carried
//! state (convolution histories, transposed-convolution overlaps, LSTM states): 400 ms
//! in, 400 ms out, identical to processing the whole signal at once and with no
//! look-ahead. The receiver loads only the decoder half, the transmitter only the
//! encoder half.
//!
//! ## Model weights
//!
//! `facebook/encodec_24khz` (`model.safetensors`, 93 MB) is downloaded once with
//! `decdrm models download encodec`; see [`weights`] for the directory rules
//! (`$DECDRM_MODELS`, else `models` next to the executable).

#![forbid(unsafe_code)]
// The crate docs describe the `encodec` build; without the feature most items they link
// to are compiled out.
#![cfg_attr(not(feature = "encodec"), allow(rustdoc::broken_intra_doc_links))]

pub mod config;
mod crc;
mod error;
pub mod framing;
pub mod plan;
mod sha256;
pub mod weights;

#[cfg(feature = "encodec")]
pub mod dac;
#[cfg(feature = "encodec")]
mod decoder;
#[cfg(feature = "encodec")]
mod encoder;
#[cfg(feature = "encodec")]
pub mod model;

pub use config::{
    Bandwidth, CODE_BITS, CODEBOOK_SIZE, ConfigError, EncodecConfig, FRAME_RATE, FRAME_SAMPLES, FRAMES_PER_SUPER_FRAME,
    LATENT_DIM, MAX_CODEBOOKS, SAMPLE_RATE, SUPER_FRAME_SAMPLES,
};
pub use crc::Crc8;
pub use error::EncodecError;
pub use framing::{Codes, FrameLayout, FramingError, Unpacked};
pub use plan::{PlanError, choose_config};
pub use weights::{WeightsNotFound, find_weights};

#[cfg(feature = "encodec")]
pub use decoder::{
    Concealment, CrcPolicy, DecodedSuperFrame, DecoderStats, EncodecDecoder, FADE_FRAMES, HOLD_FRAMES,
    MAX_INTERPOLATION_FRAMES, open_decoder,
};
#[cfg(feature = "encodec")]
pub use encoder::{EncodecDrmEncoder, EncodecEncoder};
#[cfg(feature = "encodec")]
pub use model::{EncodecModel, ModelParts};

/// Whether this build contains the codec itself (the `encodec` feature); without it,
/// EnCodec services can be planned, framed and described but not encoded or decoded.
pub const BUILT_IN: bool = cfg!(feature = "encodec");
