//! The transmitter side: 24 kHz mono PCM → EnCodec codes → DRM audio super frames.

use crate::config::{EncodecConfig, FRAMES_PER_SUPER_FRAME, SUPER_FRAME_SAMPLES};
use crate::error::EncodecError;
use crate::framing::{Codes, FrameLayout};
use crate::model::{EncodecModel, EncoderState, ModelParts};
use std::sync::Arc;

/// A streaming EnCodec encoder: PCM at 24 kHz, mono, in [-1, 1], in any whole number of
/// 320-sample frames per call (resample other rates beforehand, e.g. with
/// `decdrm_io::Resampler`).
pub struct EncodecEncoder {
    model: Arc<EncodecModel>,
    state: EncoderState,
    codebooks: usize,
}

impl EncodecEncoder {
    /// An encoder producing `codebooks` codes per frame (2, 4, 8, 16 or 32 for the
    /// trained bandwidths).
    pub fn new(model: Arc<EncodecModel>, codebooks: usize) -> Result<Self, EncodecError> {
        let state = model.encoder_state()?;
        Ok(Self { model, state, codebooks })
    }

    /// Encode the next piece of the stream; frame-major codes.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u16>, EncodecError> {
        self.model.encode(&mut self.state, pcm, self.codebooks)
    }

    /// Start a new stream.
    pub fn reset(&mut self) -> Result<(), EncodecError> {
        self.state = self.model.encoder_state()?;
        Ok(())
    }

    pub fn codebooks(&self) -> usize {
        self.codebooks
    }
}

/// Encoder plus DRM framing: 400 ms of PCM in, one audio super frame out.
///
/// ```no_run
/// # fn main() -> Result<(), decdrm_encodec::EncodecError> {
/// use decdrm_encodec::{Bandwidth, EncodecConfig, EncodecDrmEncoder, SUPER_FRAME_SAMPLES};
/// let config = EncodecConfig::new(Bandwidth::Kbps6, 3, 0)?;
/// let mut enc = EncodecDrmEncoder::open(config)?; // weights from the default location
/// let pcm = vec![0.0f32; SUPER_FRAME_SAMPLES]; // 400 ms at 24 kHz
/// let super_frame = enc.super_frame(&pcm, 330)?; // bytes of the audio stream
/// # let _ = super_frame; Ok(()) }
/// ```
pub struct EncodecDrmEncoder {
    encoder: EncodecEncoder,
    layout: FrameLayout,
}

impl EncodecDrmEncoder {
    pub fn new(model: Arc<EncodecModel>, config: EncodecConfig) -> Result<Self, EncodecError> {
        Ok(Self { encoder: EncodecEncoder::new(model, config.codebooks())?, layout: FrameLayout::new(config) })
    }

    /// With the encoder half of the model from the default location (see
    /// [`crate::weights`]).
    pub fn open(config: EncodecConfig) -> Result<Self, EncodecError> {
        Self::new(EncodecModel::load_default(ModelParts::Encoder)?, config)
    }

    pub fn config(&self) -> EncodecConfig {
        self.layout.config
    }

    /// Encode 400 ms ([`SUPER_FRAME_SAMPLES`] samples) into an audio super frame of
    /// `len` bytes.
    pub fn super_frame(&mut self, pcm: &[f32], len: usize) -> Result<Vec<u8>, EncodecError> {
        let codes = self.encode_codes(pcm)?;
        Ok(self.layout.pack(&codes, len)?)
    }

    /// Encode 400 ms into codes only.
    pub fn encode_codes(&mut self, pcm: &[f32]) -> Result<Codes, EncodecError> {
        if pcm.len() != SUPER_FRAME_SAMPLES {
            return Err(EncodecError::InvalidInput(format!(
                "{} samples given, a super frame is {SUPER_FRAME_SAMPLES} ({FRAMES_PER_SUPER_FRAME} frames)",
                pcm.len()
            )));
        }
        Ok(Codes::from_vec(self.encoder.codebooks(), self.encoder.encode(pcm)?)?)
    }
}
