//! The receiver side: audio super frame → CRC checks → latents (with concealment) →
//! 24 kHz PCM.
//!
//! # Errors and concealment
//!
//! Regions that failed their CRC are first repaired from the repeated copy where there
//! is one ([`crate::framing`]). What remains is handled by the [`CrcPolicy`] — by
//! default ([`CrcPolicy::Adaptive`]) every code is used as received unless the super
//! frame is garbage; then a frame whose base layer (codebooks 0–1) failed is lost —
//! and lost frames are concealed in the latent domain, the input of DAC's decoder
//! network, according to [`Concealment`] (default: interpolation between the
//! neighbouring good frames). The measurements behind the defaults are in the crate
//! docs.
//!
//! # Joint stereo
//!
//! The mid and side signals have a decoder network state each and are decoded side by
//! side on the thread pool; the output is L = M + S, R = M − S. The CRC policy and the
//! concealment apply to each signal on its own, with one link: a frame whose mid signal
//! is lost loses its side signal too, so both are concealed alike. A side signal lost on
//! its own is concealed like a mono signal — bridged, then faded out — which fades the
//! sound to mono rather than to silence.
//!
//! # Delay
//!
//! DAC's decoder looks [`DECODER_LAG`] samples (131 ms) ahead, so the PCM of a super
//! frame comes out that much later: a decoder starts with that much silence and then
//! returns 400 ms per super frame. The output gains of the concealment follow their
//! frames through the delay.

use crate::config::{DacConfig, FRAME_SAMPLES, FRAMES_PER_SUPER_FRAME, LATENT_DIM, SAMPLE_RATE, SUPER_FRAME_SAMPLES};
use crate::error::DacError;
use crate::framing::{Codes, FrameLayout, Unpacked, layer_codebooks, unpack_super_frame};
use crate::model::{DECODER_LAG, DacModel, DecoderState, ModelParts};
use decdrm_codecs::{CodecError, DrmAudioDecoder, PcmFrame};
use std::collections::VecDeque;
use std::sync::Arc;

/// Frames (40 ms) a gap is bridged by repeating the last latent before fading out.
pub const HOLD_FRAMES: usize = 3;
/// Frames (80 ms) of the fade to silence after the hold.
pub const FADE_FRAMES: usize = 6;
/// Longest gap (120 ms) that is interpolated between the good frames around it.
pub const MAX_INTERPOLATION_FRAMES: usize = 9;

/// What the decoder does with the codes of a region that failed its CRC (in every
/// copy). Measured in `examples/dac_errors.rs` (see the crate docs).
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
    TrustEnhancement,
    /// Every code as received while most of the super frame is intact; when at least
    /// [`GARBAGE_FRACTION`] of its regions failed — garbage, e.g. while the signal is
    /// being lost — as [`CrcPolicy::TrustEnhancement`]. With DAC, the one or two wrong
    /// codes a burst leaves in a region cost less than concealing its frames, while a
    /// garbage super frame is better concealed.
    #[default]
    Adaptive,
}

/// Share of a super frame's regions that must fail for [`CrcPolicy::Adaptive`] to take
/// it as garbage. Random data passes CRC-8 once in 256, so garbage fails about 99.6 % of
/// its regions; bursts of 10⁻² per bit, far more than audio survives, about half.
pub const GARBAGE_FRACTION: f64 = 0.9;

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

/// Counters of a decoder. The frame counts are of the mono or mid signal; the region
/// counts include the side signal's regions.
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
    /// Joint stereo: frames whose side signal was lost (concealed).
    pub side_frames_concealed: u64,
}

/// One decoded super frame.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedSuperFrame {
    /// 400 ms of 24 kHz PCM: mono, or interleaved stereo (L, R, L, R, …).
    pub pcm: Vec<f32>,
    /// Channels of `pcm`: 1 or 2.
    pub channels: usize,
    /// Codebooks used by each frame of the mono or mid signal (0 = concealed).
    pub depth: [usize; FRAMES_PER_SUPER_FRAME],
    /// Joint stereo: codebooks used by each frame of the side signal (0 = concealed).
    pub side_depth: Option<[usize; FRAMES_PER_SUPER_FRAME]>,
    /// Frames concealed.
    pub concealed: usize,
    /// Frames decoded with fewer codebooks than sent.
    pub degraded: usize,
}

/// The decoding state of one signal (mono, mid or side): the network's streaming state
/// and the concealment's memory.
struct Channel {
    state: DecoderState,
    layout: FrameLayout,
    /// Latent of the last frame given to the network, if it came from received codes
    /// (directly, interpolated or repeated).
    last_latent: Option<Vec<f32>>,
    /// Consecutive concealed frames so far.
    gap: usize,
    /// Output gain at the end of the last frame.
    gain: f32,
    /// Gains of the samples the network has not put out yet (its look-ahead).
    gains: VecDeque<f32>,
    /// PCM waiting to go out: starts with the decoder's delay as silence.
    pending: VecDeque<f32>,
}

/// A streaming DAC decoder for one DRM service.
pub struct DacDecoder {
    model: Arc<DacModel>,
    config: DacConfig,
    /// The mono or mid signal.
    mid: Channel,
    /// Joint stereo: the side signal.
    side: Option<Channel>,
    policy: CrcPolicy,
    concealment: Concealment,
    stats: DecoderStats,
}

impl DacDecoder {
    pub fn new(model: Arc<DacModel>, config: DacConfig) -> Result<Self, DacError> {
        let side = config.side_config().map(|s| Channel::new(&model, s)).transpose()?;
        Ok(Self {
            mid: Channel::new(&model, config)?,
            side,
            model,
            config,
            policy: CrcPolicy::default(),
            concealment: Concealment::default(),
            stats: DecoderStats::default(),
        })
    }

    /// A decoder for the service whose SDC type 9 bytes (after Short Id and Stream Id)
    /// are `type9`, with the decoder half of the model from the default location (see
    /// [`crate::weights`]).
    pub fn from_type9_bytes(type9: &[u8]) -> Result<Self, DacError> {
        let config = DacConfig::from_type9_bytes(type9)?;
        Self::new(DacModel::load_default(ModelParts::Decoder)?, config)
    }

    pub fn config(&self) -> DacConfig {
        self.config
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
    pub fn decode_super_frame(&mut self, sf: &[u8]) -> Result<DecodedSuperFrame, DacError> {
        let (mid, side) = unpack_super_frame(self.config, sf)?;
        let depth = self.usable_depth(&mid, self.mid.layout, true);
        let side_layout = self.side.as_ref().map(|s| s.layout);
        let side = side.zip(side_layout).map(|(u, layout)| {
            let mut d = self.usable_depth(&u, layout, false);
            for (s, &m) in d.iter_mut().zip(&depth) {
                if m == 0 {
                    *s = 0;
                }
            }
            (u.codes, d)
        });
        self.decode_frames(&mid.codes, depth, side.as_ref().map(|(c, d)| (c, *d)))
    }

    /// 400 ms of concealment (nothing received).
    pub fn conceal_super_frame(&mut self) -> Result<DecodedSuperFrame, DacError> {
        let lost = [0; FRAMES_PER_SUPER_FRAME];
        let side = self.side.as_ref().map(|s| Codes::new(s.layout.codebooks()));
        self.decode_frames(&Codes::new(self.mid.layout.codebooks()), lost, side.as_ref().map(|c| (c, lost)))
    }

    /// The codebooks each frame of a signal (laid out by `layout`) decodes, by the CRC
    /// results and the policy; counts the regions, and with `count_frames` the
    /// unverified frames.
    fn usable_depth(&mut self, u: &Unpacked, layout: FrameLayout, count_frames: bool) -> [usize; FRAMES_PER_SUPER_FRAME] {
        self.stats.regions_failed += (u.failed + u.repaired) as u64;
        self.stats.regions_repaired += u.repaired as u64;
        let all = layout.codebooks();
        let group = layout.config.group_frames;
        let mut depth = u.depth;
        let regions = u.region_ok.iter().map(Vec::len).sum::<usize>().max(1);
        let garbage = u.failed as f64 >= GARBAGE_FRACTION * regions as f64;
        match self.policy {
            CrcPolicy::Ignore => depth = [all; FRAMES_PER_SUPER_FRAME],
            CrcPolicy::Adaptive if !garbage => depth = [all; FRAMES_PER_SUPER_FRAME],
            CrcPolicy::Strict => {}
            CrcPolicy::TrustEnhancement | CrcPolicy::Adaptive => {
                for (f, d) in depth.iter_mut().enumerate() {
                    if u.region_ok[0][f / group] {
                        *d = all;
                    }
                }
            }
        }
        // Frames that use codes of a failed region.
        if count_frames {
            let unverified = (0..FRAMES_PER_SUPER_FRAME)
                .filter(|&f| (0..layout.layers()).any(|l| layer_codebooks(l).start < depth[f] && !u.region_ok[l][f / group]))
                .count();
            self.stats.frames_unverified += unverified as u64;
        }
        depth
    }

    fn decode_frames(
        &mut self,
        codes: &Codes,
        depth: [usize; FRAMES_PER_SUPER_FRAME],
        side: Option<(&Codes, [usize; FRAMES_PER_SUPER_FRAME])>,
    ) -> Result<DecodedSuperFrame, DacError> {
        let (model, concealment) = (&*self.model, self.concealment);
        let mid = &mut self.mid;
        let (pcm, side_depth) = match (self.side.as_mut(), side) {
            (Some(side), Some((side_codes, side_depth))) => {
                // (Rust note: the two closures borrow different channels mutably, which
                // `rayon::join` may run on two threads at once.)
                let (m, s) = rayon::join(
                    || mid.decode(model, codes, &depth, concealment),
                    || side.decode(model, side_codes, &side_depth, concealment),
                );
                let (m, s) = (m?, s?);
                let pcm = m.iter().zip(&s).flat_map(|(&m, &s)| [m + s, m - s]).collect();
                (pcm, Some(side_depth))
            }
            _ => (mid.decode(model, codes, &depth, concealment)?, None),
        };

        let concealed = depth.iter().filter(|&&d| d == 0).count();
        let degraded = depth.iter().filter(|&&d| d > 0 && d < self.config.codebooks()).count();
        let s = &mut self.stats;
        s.super_frames += 1;
        s.frames_concealed += concealed as u64;
        s.frames_degraded += degraded as u64;
        s.frames_full += (FRAMES_PER_SUPER_FRAME - concealed - degraded) as u64;
        s.side_frames_concealed += side_depth.map_or(0, |d| d.iter().filter(|&&d| d == 0).count()) as u64;
        Ok(DecodedSuperFrame { pcm, channels: self.config.channels(), depth, side_depth, concealed, degraded })
    }
}

impl Channel {
    fn new(model: &DacModel, config: DacConfig) -> Result<Self, DacError> {
        Ok(Self {
            state: model.decoder_state()?,
            layout: FrameLayout::new(config),
            last_latent: None,
            gap: 0,
            gain: 0.0,
            gains: VecDeque::new(),
            pending: std::iter::repeat_n(0.0, DECODER_LAG).collect(),
        })
    }

    /// 400 ms of this signal's PCM from the codes and usable depths of a super frame.
    fn decode(
        &mut self,
        model: &DacModel,
        codes: &Codes,
        depth: &[usize; FRAMES_PER_SUPER_FRAME],
        concealment: Concealment,
    ) -> Result<Vec<f32>, DacError> {
        let mut latents = vec![0f32; FRAMES_PER_SUPER_FRAME * LATENT_DIM];
        for (f, out) in latents.as_chunks_mut::<LATENT_DIM>().0.iter_mut().enumerate() {
            if depth[f] > 0 {
                model.latent(codes.frame(f), depth[f], out)?;
            }
        }
        let mut gains = [1f32; FRAMES_PER_SUPER_FRAME];
        let last_valid = self.fill_gaps(depth, &mut latents, &mut gains, concealment);
        // Gain ramps across each frame from the previous frame's gain to its own; they
        // wait with the frames' samples for the network's look-ahead.
        let mut g0 = self.gain;
        for &g1 in &gains {
            self.gains.extend((0..FRAME_SAMPLES).map(|i| g0 + (g1 - g0) * (i + 1) as f32 / FRAME_SAMPLES as f32));
            g0 = g1;
        }
        self.gain = g0;
        let out = model.decode_latents_stream(&mut self.state, &latents)?;
        let n = out.len().min(self.gains.len());
        for (s, g) in out.into_iter().zip(self.gains.drain(..n)) {
            self.pending.push_back(s * g);
        }
        let take = SUPER_FRAME_SAMPLES.min(self.pending.len());
        let mut pcm: Vec<f32> = self.pending.drain(..take).collect();
        pcm.resize(SUPER_FRAME_SAMPLES, 0.0);
        self.last_latent = last_valid.then(|| latents[latents.len() - LATENT_DIM..].to_vec());
        Ok(pcm)
    }

    /// Fill the latents and output gains of the lost frames (`depth` 0). Returns whether
    /// the last frame's latent derives from received codes.
    fn fill_gaps(
        &mut self,
        depth: &[usize; FRAMES_PER_SUPER_FRAME],
        latents: &mut [f32],
        gains: &mut [f32; FRAMES_PER_SUPER_FRAME],
        concealment: Concealment,
    ) -> bool {
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
            let interpolate =
                concealment == Concealment::Interpolate && self.gap == 0 && end < n && len <= MAX_INTERPOLATION_FRAMES;
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
                        gains[f + i] = match concealment {
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

impl DrmAudioDecoder for DacDecoder {
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

    /// `DAC 6 kbit/s (8 codebooks), 24 kHz mono` or `DAC joint stereo, 12 kbit/s mid +
    /// 1.5 kbit/s side, 24 kHz`.
    fn describe(&self) -> String {
        let bw = self.config.bandwidth;
        match self.config.side {
            None => format!("DAC {bw} ({} codebooks), 24 kHz mono", bw.codebooks()),
            Some(_) => format!("DAC joint stereo, {}, 24 kHz", self.config.rates()),
        }
    }
}

/// A decoded super frame as the engine's PCM block: flagged as concealed when any of
/// its frames was.
fn pcm_frame(d: DecodedSuperFrame) -> PcmFrame {
    PcmFrame { sample_rate: SAMPLE_RATE, channels: d.channels as _, samples: d.pcm, concealed: d.concealed > 0 }
}

/// Open the decoder of a DAC service for the receiver engine, from the service's
/// SDC type 9 bytes (`AudioParams::type9_bytes`).
pub fn open_decoder(type9_bytes: &[u8]) -> Result<Box<dyn DrmAudioDecoder>, CodecError> {
    let config = DacConfig::from_type9_bytes(type9_bytes).map_err(|e| CodecError::InvalidConfig(e.to_string()))?;
    let model = DacModel::load_default(ModelParts::Decoder).map_err(|e| CodecError::Unsupported(e.to_string()))?;
    let decoder = DacDecoder::new(model, config).map_err(|e| CodecError::Unsupported(e.to_string()))?;
    Ok(Box::new(decoder))
}
