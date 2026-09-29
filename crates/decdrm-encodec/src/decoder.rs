//! The receiver side: audio super frame → CRC checks → latents (with concealment) →
//! 24 kHz PCM.
//!
//! # Errors and concealment
//!
//! Regions that failed their CRC are first repaired from the repeated copy where there
//! is one ([`crate::framing`]). What remains is handled by the [`CrcPolicy`] — by
//! default a frame whose base layer (codebooks 0–1) failed is lost, while failed
//! enhancement layers are used as received — and lost frames are concealed in the
//! latent domain, the input of EnCodec's decoder network, according to
//! [`Concealment`] (default: interpolation between the neighbouring good frames). Both
//! defaults were the best choices in the measurements quoted in the crate docs.

use crate::config::{
    Bandwidth, EncodecConfig, FRAME_SAMPLES, FRAMES_PER_SUPER_FRAME, LATENT_DIM, SAMPLE_RATE,
};
use crate::error::EncodecError;
use crate::framing::{Codes, FrameLayout, layer_codebooks};
use crate::model::{DecoderState, EncodecModel, ModelParts};
use decdrm_codecs::{CodecError, DrmAudioDecoder, PcmFrame};
use std::sync::Arc;

/// Frames (40 ms) a gap is bridged by repeating the last latent before fading out.
pub const HOLD_FRAMES: usize = 3;
/// Frames (80 ms) of the fade to silence after the hold.
pub const FADE_FRAMES: usize = 6;
/// Longest gap (120 ms) that is interpolated between the good frames around it.
pub const MAX_INTERPOLATION_FRAMES: usize = 9;

/// What the decoder does with the codes of a region that failed its CRC (in every
/// copy). Measured in `examples/encodec_errors.rs` (see the crate docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CrcPolicy {
    /// Use every code as received, as if there were no CRCs. Fine for sparse bursts,
    /// but a corrupt super frame decodes to loud babble.
    Ignore,
    /// A frame uses only the layers below its first failed one; a failed base layer
    /// makes it lost (concealed). The worst choice under burst errors: a burst spoils
    /// one or two codes of a region, dropping loses all of them and every layer above.
    Strict,
    /// A failed base layer makes the frame lost (concealed); failed enhancement layers
    /// are used anyway — a wrong fine code costs less than dropping the region.
    #[default]
    TrustEnhancement,
}

/// What replaces frames that are lost (base layer failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Concealment {
    /// Interpolate the latent linearly between the good frames on either side when the
    /// gap lies within one super frame and is at most [`MAX_INTERPOLATION_FRAMES`]
    /// long; otherwise as [`Concealment::Repeat`].
    #[default]
    Interpolate,
    /// Repeat the last latent for [`HOLD_FRAMES`], then fade out over
    /// [`FADE_FRAMES`] (keeping the latent), then silence.
    Repeat,
    /// Silence (13 ms ramps at the edges).
    Mute,
}

/// Counters of a decoder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecoderStats {
    /// Super frames decoded (or concealed as a whole).
    pub super_frames: u64,
    /// Frames decoded with every codebook.
    pub frames_full: u64,
    /// Frames decoded with fewer codebooks because an enhancement layer failed.
    pub frames_degraded: u64,
    /// Frames whose base layer was lost.
    pub frames_concealed: u64,
    /// Frames decoded with codes of a region that failed its CRC (the policy trusted
    /// them).
    pub frames_unverified: u64,
    /// CRC regions whose first copy failed.
    pub regions_failed: u64,
    /// Of those, regions recovered from the repeated copy.
    pub regions_repaired: u64,
}

/// One decoded super frame.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedSuperFrame {
    /// 400 ms of 24 kHz mono PCM.
    pub pcm: Vec<f32>,
    /// Codebooks used by each frame (0 = concealed).
    pub depth: [usize; FRAMES_PER_SUPER_FRAME],
    /// Frames concealed.
    pub concealed: usize,
    /// Frames decoded with fewer codebooks than sent.
    pub degraded: usize,
}

/// A streaming EnCodec decoder for one DRM service.
pub struct EncodecDecoder {
    model: Arc<EncodecModel>,
    state: DecoderState,
    layout: FrameLayout,
    policy: CrcPolicy,
    concealment: Concealment,
    /// Latent of the last frame given to the network, if it came from received codes
    /// (directly, interpolated or repeated).
    last_latent: Option<Vec<f32>>,
    /// Consecutive concealed frames so far.
    gap: usize,
    /// Output gain at the end of the last frame.
    gain: f32,
    stats: DecoderStats,
}

impl EncodecDecoder {
    pub fn new(model: Arc<EncodecModel>, config: EncodecConfig) -> Result<Self, EncodecError> {
        let state = model.decoder_state()?;
        Ok(Self {
            model,
            state,
            layout: FrameLayout::new(config),
            policy: CrcPolicy::default(),
            concealment: Concealment::default(),
            last_latent: None,
            gap: 0,
            gain: 0.0,
            stats: DecoderStats::default(),
        })
    }

    /// A decoder for the service whose SDC type 9 bytes (after Short Id and Stream Id)
    /// are `type9`, with the decoder half of the model from the default location (see
    /// [`crate::weights`]).
    pub fn from_type9_bytes(type9: &[u8]) -> Result<Self, EncodecError> {
        let config = EncodecConfig::from_type9_bytes(type9)?;
        Self::new(EncodecModel::load_default(ModelParts::Decoder)?, config)
    }

    pub fn config(&self) -> EncodecConfig {
        self.layout.config
    }

    pub fn set_crc_policy(&mut self, p: CrcPolicy) {
        self.policy = p;
    }

    pub fn set_concealment(&mut self, c: Concealment) {
        self.concealment = c;
    }

    pub fn stats(&self) -> DecoderStats {
        self.stats
    }

    /// Decode one audio super frame (the stream's logical frame without text message
    /// bytes).
    pub fn decode_super_frame(&mut self, sf: &[u8]) -> Result<DecodedSuperFrame, EncodecError> {
        let u = self.layout.unpack(sf)?;
        self.stats.regions_failed += (u.failed + u.repaired) as u64;
        self.stats.regions_repaired += u.repaired as u64;
        let all = self.layout.codebooks();
        let group = self.layout.config.group_frames;
        let mut depth = u.depth;
        match self.policy {
            CrcPolicy::Ignore => depth = [all; FRAMES_PER_SUPER_FRAME],
            CrcPolicy::Strict => {}
            CrcPolicy::TrustEnhancement => {
                for (f, d) in depth.iter_mut().enumerate() {
                    if u.region_ok[0][f / group] {
                        *d = all;
                    }
                }
            }
        }
        // Frames that use codes of a failed region.
        let unverified = (0..FRAMES_PER_SUPER_FRAME)
            .filter(|&f| {
                (0..self.layout.layers())
                    .any(|l| layer_codebooks(l).start < depth[f] && !u.region_ok[l][f / group])
            })
            .count();
        self.stats.frames_unverified += unverified as u64;
        self.decode_frames(&u.codes, depth)
    }

    /// 400 ms of concealment (nothing received).
    pub fn conceal_super_frame(&mut self) -> Result<DecodedSuperFrame, EncodecError> {
        self.decode_frames(&Codes::new(self.layout.codebooks()), [0; FRAMES_PER_SUPER_FRAME])
    }

    fn decode_frames(&mut self, codes: &Codes, depth: [usize; FRAMES_PER_SUPER_FRAME]) -> Result<DecodedSuperFrame, EncodecError> {
        let mut latents = vec![0f32; FRAMES_PER_SUPER_FRAME * LATENT_DIM];
        for (f, out) in latents.as_chunks_mut::<LATENT_DIM>().0.iter_mut().enumerate() {
            if depth[f] > 0 {
                self.model.latent(codes.frame(f), depth[f], out)?;
            }
        }
        let mut gains = [1f32; FRAMES_PER_SUPER_FRAME];
        let last_valid = self.fill_gaps(&depth, &mut latents, &mut gains);
        let mut pcm = self.model.decode_latents(&mut self.state, &latents)?;

        // Gain ramps across each frame from the previous frame's gain to its own.
        let mut g0 = self.gain;
        for (chunk, &g1) in pcm.as_chunks_mut::<FRAME_SAMPLES>().0.iter_mut().zip(&gains) {
            if g0 != 1.0 || g1 != 1.0 {
                for (i, s) in chunk.iter_mut().enumerate() {
                    *s *= g0 + (g1 - g0) * (i + 1) as f32 / FRAME_SAMPLES as f32;
                }
            }
            g0 = g1;
        }
        self.gain = g0;
        self.last_latent = last_valid.then(|| latents[latents.len() - LATENT_DIM..].to_vec());

        let concealed = depth.iter().filter(|&&d| d == 0).count();
        let degraded = depth.iter().filter(|&&d| d > 0 && d < self.layout.codebooks()).count();
        let s = &mut self.stats;
        s.super_frames += 1;
        s.frames_concealed += concealed as u64;
        s.frames_degraded += degraded as u64;
        s.frames_full += (FRAMES_PER_SUPER_FRAME - concealed - degraded) as u64;
        Ok(DecodedSuperFrame { pcm, depth, concealed, degraded })
    }

    /// Fill the latents and output gains of the lost frames (`depth` 0). Returns whether
    /// the last frame's latent derives from received codes.
    fn fill_gaps(&mut self, depth: &[usize; FRAMES_PER_SUPER_FRAME], latents: &mut [f32], gains: &mut [f32; FRAMES_PER_SUPER_FRAME]) -> bool {
        const D: usize = LATENT_DIM;
        let n = FRAMES_PER_SUPER_FRAME;
        let mut valid = self.last_latent.is_some();
        let mut f = 0;
        while f < n {
            if depth[f] > 0 {
                self.gap = 0;
                valid = true;
                f += 1;
                continue;
            }
            let end = (f..n).find(|&i| depth[i] > 0).unwrap_or(n);
            let len = end - f;
            // The latent before the gap: the previous frame, possibly of the previous
            // super frame.
            let prev: Option<Vec<f32>> =
                if f > 0 { valid.then(|| latents[(f - 1) * D..f * D].to_vec()) } else { self.last_latent.clone() };
            let interpolate = self.concealment == Concealment::Interpolate
                && self.gap == 0
                && end < n
                && len <= MAX_INTERPOLATION_FRAMES;
            match prev {
                Some(prev) if interpolate => {
                    let next = latents[end * D..(end + 1) * D].to_vec();
                    for i in 0..len {
                        let a = (i + 1) as f32 / (len + 1) as f32;
                        let out = &mut latents[(f + i) * D..(f + i + 1) * D];
                        for ((o, p), q) in out.iter_mut().zip(&prev).zip(&next) {
                            *o = (1.0 - a) * p + a * q;
                        }
                    }
                }
                prev => {
                    for i in 0..len {
                        // Frames into the gap (1 = its first frame).
                        let k = self.gap + i + 1;
                        if let Some(p) = &prev {
                            latents[(f + i) * D..(f + i + 1) * D].copy_from_slice(p);
                        }
                        gains[f + i] = match self.concealment {
                            _ if prev.is_none() => 0.0,
                            Concealment::Mute => 0.0,
                            _ if k <= HOLD_FRAMES => 1.0,
                            _ if k < HOLD_FRAMES + FADE_FRAMES => 1.0 - (k - HOLD_FRAMES) as f32 / FADE_FRAMES as f32,
                            _ => 0.0,
                        };
                    }
                    self.gap += len;
                    valid = prev.is_some();
                }
            }
            f = end;
        }
        valid
    }
}

impl DrmAudioDecoder for EncodecDecoder {
    /// `frame` is the whole audio super frame (the CRCs are inside it; `crc` is unused).
    fn decode(&mut self, frame: &[u8], _crc: Option<u8>) -> Result<PcmFrame, CodecError> {
        let d = if frame.is_empty() { self.conceal_super_frame() } else { self.decode_super_frame(frame) }
            .map_err(|e| CodecError::CorruptFrame(e.to_string()))?;
        Ok(pcm_frame(d))
    }

    fn conceal(&mut self) -> Result<PcmFrame, CodecError> {
        let d = self.conceal_super_frame().map_err(|e| CodecError::CorruptFrame(e.to_string()))?;
        Ok(pcm_frame(d))
    }

    fn describe(&self) -> String {
        let bw: Bandwidth = self.layout.config.bandwidth;
        format!("EnCodec {bw} ({} codebooks), 24 kHz mono", bw.codebooks())
    }
}

/// A decoded super frame as the engine's PCM block: flagged as concealed when any of
/// its frames was.
fn pcm_frame(d: DecodedSuperFrame) -> PcmFrame {
    PcmFrame { sample_rate: SAMPLE_RATE, channels: 1, samples: d.pcm, concealed: d.concealed > 0 }
}

/// Open the decoder of an EnCodec service for the receiver engine, from the service's
/// SDC type 9 bytes (`AudioParams::type9_bytes`).
pub fn open_decoder(type9_bytes: &[u8]) -> Result<Box<dyn DrmAudioDecoder>, CodecError> {
    let config = EncodecConfig::from_type9_bytes(type9_bytes).map_err(|e| CodecError::InvalidConfig(e.to_string()))?;
    let model = EncodecModel::load_default(ModelParts::Decoder).map_err(|e| CodecError::Unsupported(e.to_string()))?;
    let decoder = EncodecDecoder::new(model, config).map_err(|e| CodecError::Unsupported(e.to_string()))?;
    Ok(Box::new(decoder))
}
