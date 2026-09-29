//! The crate-wide error type.
//!
//! Every fallible function in this crate returns [`Result<T>`](crate::Result), an alias for
//! `std::result::Result<T, Error>`. The `?` operator converts library errors (hound, cpal, ...)
//! into [`Error`] automatically through the `From` impls that `thiserror` generates for the
//! `#[from]` fields below.

use std::path::PathBuf;

use crate::Direction;

/// Errors produced by `decdrm-io`.
///
/// The enum is `#[non_exhaustive]`: new variants may be added without a breaking change, so a
/// `match` on it needs a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Opening, creating, reading or writing a file failed at the operating-system level.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file concerned.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The file could not be demultiplexed or decoded (corrupt, truncated or an unsupported
    /// container/codec). Produced by the symphonia-based [`FileReader`](crate::FileReader).
    #[error("{}: {source}", path.display())]
    Decode {
        /// The file concerned.
        path: PathBuf,
        /// The decoder's error.
        #[source]
        source: symphonia::core::errors::Error,
    },

    /// The file was parsed but cannot be used: no audio track, or the sample rate / channel
    /// count is missing or out of range.
    #[error("{}: {reason}", path.display())]
    UnsupportedFile {
        /// The file concerned.
        path: PathBuf,
        /// Human-readable explanation.
        reason: String,
    },

    /// A caller-supplied argument was out of range (zero channels, zero sample rate, a sample
    /// slice that is not a whole number of frames, an unsupported bit depth, ...).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The WAV writer (hound) failed.
    #[error("WAV writer: {0}")]
    Wav(#[from] hound::Error),

    /// The FLAC encoder (flacenc) failed.
    #[error("FLAC encoder: {0}")]
    Flac(String),

    /// The output would exceed a hard limit of the container (a RIFF/WAV file cannot exceed
    /// 4 GiB; use FLAC for long recordings).
    #[error("{0}")]
    FileTooLarge(String),

    /// The resampler (rubato) could not be built or failed while processing.
    #[error("resampler: {0}")]
    Resampler(String),

    /// The audio backend (WASAPI on Windows, ALSA on Linux, via cpal) reported an error.
    #[error("audio backend: {0}")]
    Audio(#[from] cpal::Error),

    /// No sound-card device matched the requested name.
    #[error("no {direction} device matching {name:?} (available: {available})")]
    DeviceNotFound {
        /// Input or output.
        direction: Direction,
        /// The name that was searched for.
        name: String,
        /// Comma-separated list of the device names that do exist.
        available: String,
    },

    /// More than one device matched a partial name; the caller must be more specific.
    #[error("{direction} device name {name:?} is ambiguous, it matches: {matches}")]
    AmbiguousDevice {
        /// Input or output.
        direction: Direction,
        /// The name that was searched for.
        name: String,
        /// Comma-separated list of the matching device names.
        matches: String,
    },

    /// The host has no default device of this direction (e.g. no sound card at all).
    #[error("no default {0} device")]
    NoDefaultDevice(Direction),

    /// The device offers no stream configuration we can use (e.g. only DSD formats).
    #[error("device {0:?} offers no usable stream configuration")]
    NoUsableConfig(String),

    /// A method was called on a writer that has already been finalized.
    #[error("the writer has already been finalized")]
    Finalized,
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io { path: path.into(), source }
    }

    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Error::InvalidArgument(msg.into())
    }
}
