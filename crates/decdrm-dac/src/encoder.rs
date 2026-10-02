//! The transmitter side: 24 kHz mono PCM → DAC codes → DRM audio super frames.

use crate::config::{DacConfig, FRAMES_PER_SUPER_FRAME, SUPER_FRAME_SAMPLES};
use crate::error::DacError;
use crate::framing::{Codes, FrameLayout};
use crate::model::{DacModel, ENCODER_LEAD_IN, EncoderState, ModelParts};
use std::sync::Arc;

/// A streaming DAC encoder: PCM at 24 kHz, mono, in [-1, 1], in any whole number of
/// 320-sample frames per call (resample other rates beforehand, e.g. with
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
    encoder: DacEncoder,
    layout: FrameLayout,
}

impl DacDrmEncoder {
    pub fn new(model: Arc<DacModel>, config: DacConfig) -> Result<Self, DacError> {
        Ok(Self { encoder: DacEncoder::new(model, config.codebooks())?, layout: FrameLayout::new(config) })
    }

    /// With the encoder half of the model from the default location (see
    /// [`crate::weights`]).
    pub fn open(config: DacConfig) -> Result<Self, DacError> {
        Self::new(DacModel::load_default(ModelParts::Encoder)?, config)
    }

    pub fn config(&self) -> DacConfig {
        self.layout.config
    }

    /// Encode 400 ms ([`SUPER_FRAME_SAMPLES`] samples) into an audio super frame of
    /// `len` bytes.
    pub fn super_frame(&mut self, pcm: &[f32], len: usize) -> Result<Vec<u8>, DacError> {
        let codes = self.encode_codes(pcm)?;
        Ok(self.layout.pack(&codes, len)?)
    }

    /// Encode 400 ms into codes only.
    pub fn encode_codes(&mut self, pcm: &[f32]) -> Result<Codes, DacError> {
        if pcm.len() != SUPER_FRAME_SAMPLES {
            return Err(DacError::InvalidInput(format!(
                "{} samples given, a super frame is {SUPER_FRAME_SAMPLES} ({FRAMES_PER_SUPER_FRAME} frames)",
                pcm.len()
            )));
        }
        Ok(Codes::from_vec(self.encoder.codebooks(), self.encoder.encode(pcm)?)?)
    }
}
