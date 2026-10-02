//! The crate's error type.

use crate::config::ConfigError;
use crate::framing::FramingError;
use crate::weights::WeightsNotFound;
use std::path::PathBuf;

/// Errors of the DAC encoder, decoder and model loader.
///
/// Rust note: `#[from]` lets `?` convert the listed error types into this one
/// automatically; `#[non_exhaustive]` keeps adding variants a compatible change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DacError {
    /// The SDC configuration is not a usable DAC configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A super frame could not be built or read.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The model weights are not installed.
    #[error(transparent)]
    NotFound(#[from] WeightsNotFound),
    /// The weights file is unreadable or not the DAC 24 kHz model.
    #[error("{path}: {message}")]
    Weights { path: PathBuf, message: String },
    /// The model was loaded without the part (encoder or decoder) this needs.
    #[error("the DAC model was loaded without its {0}")]
    MissingPart(&'static str),
    /// Wrong input, e.g. PCM that is not a whole number of frames.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A tensor operation failed (a bug, or out of memory).
    #[cfg(feature = "dac")]
    #[error("DAC inference: {0}")]
    Candle(#[from] candle_core::Error),
}
