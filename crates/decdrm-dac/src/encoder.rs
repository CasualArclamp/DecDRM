//! The transmitter side: 24 kHz PCM (mono, or stereo as joint stereo) → DAC codes →
//! DRM audio super frames.

use crate::config::{DacConfig, FRAMES_PER_SUPER_FRAME, SUPER_FRAME_SAMPLES};
use crate::error::DacError;
use crate::framing::{Codes, pack_super_frame};
use crate::model::{DacModel, ENCODER_LEAD_IN, EncoderState, ModelParts};
use std::sync::Arc;

/// A streaming DAC encoder of one signal: PCM at 24 kHz, mono, in [-1, 1], in any
/// whole number of 320-sample frames per call (resample other rates beforehand, e.g. with
/// `decdrm_io::Resampler`).
///
/// DAC's encoder looks 104 ms ahead, so each stream starts with [`ENCODER_LEAD_IN`]
/// zero samples: then every 320 samples in complete one frame of codes, and the codes
/// lag the PCM by that lead-in (107 ms).
pub struct DacEncoder {
    model: Arc<DacModel>,
    state: EncoderState,
    codebooks: usize,
}

impl DacEncoder {
    /// An encoder producing `codebooks` codes per frame (1–32; the framing uses 2, 4,
    /// 8, 16 or 32).
    pub fn new(model: Arc<DacModel>, codebooks: usize) -> Result<Self, DacError> {
        let state = Self::start(&model)?;
        Ok(Self { model, state, codebooks })
    }

    fn start(model: &DacModel) -> Result<EncoderState, DacError> {
        let mut state = model.encoder_state()?;
        model.encode_latents(&mut state, &[0.0; ENCODER_LEAD_IN])?;
        Ok(state)
    }

    /// Encode the next piece of the stream: frame-major codes, one frame per 320
    /// samples.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u16>, DacError> {
        let latents = self.model.encode_latents(&mut self.state, pcm)?;
        self.model.quantize(&latents, self.codebooks)
    }

    /// Start a new stream.
    pub fn reset(&mut self) -> Result<(), DacError> {
        self.state = Self::start(&self.model)?;
        Ok(())
    }

    pub fn codebooks(&self) -> usize {
        self.codebooks
    }
}

/// Encoder plus DRM framing: 400 ms of PCM in, one audio super frame out.
///
/// A joint stereo configuration takes interleaved stereo PCM (L, R, L, R, …) and codes
/// the mid signal (L + R) / 2 and the side signal (L − R) / 2 with an encoder each, side
/// by side on the thread pool.
///
/// ```no_run
/// # fn main() -> Result<(), decdrm_dac::DacError> {
/// use decdrm_dac::{Bandwidth, DacConfig, DacDrmEncoder, SUPER_FRAME_SAMPLES};
/// let config = DacConfig::new(Bandwidth::Kbps6, 3, 0)?;
/// let mut enc = DacDrmEncoder::open(config)?; // weights from the default location
/// let pcm = vec![0.0f32; SUPER_FRAME_SAMPLES]; // 400 ms at 24 kHz
/// let super_frame = enc.super_frame(&pcm, 330)?; // bytes of the audio stream
/// # let _ = super_frame; Ok(()) }
/// ```
pub struct DacDrmEncoder {
    /// The mono or mid signal's encoder.
    encoder: DacEncoder,
    /// Joint stereo: the side signal's encoder.
    side: Option<DacEncoder>,
    config: DacConfig,
}

impl DacDrmEncoder {
    pub fn new(model: Arc<DacModel>, config: DacConfig) -> Result<Self, DacError> {
        let side = config.side_config().map(|s| DacEncoder::new(Arc::clone(&model), s.codebooks())).transpose()?;
        Ok(Self { encoder: DacEncoder::new(model, config.codebooks())?, side, config })
    }

    /// With the encoder half of the model from the default location (see
    /// [`crate::weights`]).
    pub fn open(config: DacConfig) -> Result<Self, DacError> {
        Self::new(DacModel::load_default(ModelParts::Encoder)?, config)
    }

    pub fn config(&self) -> DacConfig {
        self.config
    }

    /// Encode 400 ms ([`SUPER_FRAME_SAMPLES`] samples per channel; stereo interleaved)
    /// into an audio super frame of `len` bytes.
    pub fn super_frame(&mut self, pcm: &[f32], len: usize) -> Result<Vec<u8>, DacError> {
        let (mid, side) = self.encode_codes(pcm)?;
        Ok(pack_super_frame(self.config, &mid, side.as_ref(), len)?)
    }

    /// Encode 400 ms into codes only: the mono or mid signal's, and for joint stereo the
    /// side signal's.
    pub fn encode_codes(&mut self, pcm: &[f32]) -> Result<(Codes, Option<Codes>), DacError> {
        let want = SUPER_FRAME_SAMPLES * self.config.channels();
        if pcm.len() != want {
            return Err(DacError::InvalidInput(format!(
                "{} samples given, a super frame is {want} ({FRAMES_PER_SUPER_FRAME} frames, {} channel(s))",
                pcm.len(),
                self.config.channels()
            )));
        }
        let Some(side_encoder) = self.side.as_mut() else {
            return Ok((Codes::from_vec(self.encoder.codebooks(), self.encoder.encode(pcm)?)?, None));
        };
        let (mid, side): (Vec<f32>, Vec<f32>) =
            pcm.as_chunks::<2>().0.iter().map(|&[l, r]| (0.5 * (l + r), 0.5 * (l - r))).unzip();
        // (Rust note: `rayon::join` runs both closures, possibly on two threads, and
        // returns both results; each closure borrows a different encoder.)
        let encoder = &mut self.encoder;
        let (m, s) = rayon::join(|| encoder.encode(&mid), || side_encoder.encode(&side));
        Ok((Codes::from_vec(encoder.codebooks(), m?)?, Some(Codes::from_vec(side_encoder.codebooks(), s?)?)))
    }
}
