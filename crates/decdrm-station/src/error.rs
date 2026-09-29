//! Errors of the station layer.

use std::fmt;
use std::path::PathBuf;

/// Everything that can go wrong while configuring or running a station.
///
/// Rust note: `thiserror`'s `#[error("…")]` attribute writes the `Display` text, and
/// `#[source]` / `#[from]` link the underlying error so callers (e.g. `anyhow` in the
/// CLI) can print the whole chain.
#[derive(Debug, thiserror::Error)]
pub enum StationError {
    /// The configuration is inconsistent (every problem found is listed).
    #[error("{0}")]
    Config(ConfigProblems),
    /// A configuration file could not be parsed.
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    /// A file or directory named in the configuration could not be read.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// An audio input (file or sound card) failed.
    #[error("audio input of {service}: {message}")]
    Input { service: String, message: String },
    /// An audio encoder could not be created.
    #[error("audio encoder of {service}: {source}")]
    Codec {
        service: String,
        #[source]
        source: decdrm_codecs::CodecError,
    },
    /// A data application could not be set up.
    #[error("data application {what}: {message}")]
    Data { what: String, message: String },
    /// The output file or sound card failed.
    #[error("output: {0}")]
    Output(#[source] decdrm_io::Error),
    /// The transmitter chain rejected its input (a bug if the plan was validated).
    #[error("transmitter: {0}")]
    Tx(#[from] decdrm_core::tx::TxError),
    /// The MSC multiplexer rejected the logical frames (a bug if the plan was validated).
    #[error("multiplexer: {0}")]
    Mux(#[from] decdrm_core::mux::msc::MuxError),
    /// An SDC entity could not be encoded (a bug if the plan was validated).
    #[error("SDC: {0}")]
    Sdc(#[from] decdrm_core::mux::sdc::SdcError),
    /// An audio super frame could not be built (a bug if the plan was validated).
    #[error("audio super frame: {0}")]
    SuperFrame(#[from] decdrm_core::mux::audio::AudioError),
}

impl StationError {
    /// A configuration error with a single problem.
    pub(crate) fn config(problem: impl Into<String>) -> Self {
        Self::Config(ConfigProblems(vec![problem.into()]))
    }

    /// The configuration problems, if this is a configuration error.
    pub fn problems(&self) -> &[String] {
        match self {
            Self::Config(p) => &p.0,
            _ => &[],
        }
    }
}

/// The list of problems found by [`crate::StationConfig::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigProblems(pub Vec<String>);

impl fmt::Display for ConfigProblems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_slice() {
            [] => write!(f, "invalid station configuration"),
            [one] => write!(f, "invalid station configuration: {one}"),
            many => {
                write!(f, "invalid station configuration ({} problems):", many.len())?;
                for p in many {
                    write!(f, "\n  - {p}")?;
                }
                Ok(())
            }
        }
    }
}

/// Shorthand used throughout the crate.
pub type Result<T, E = StationError> = std::result::Result<T, E>;
