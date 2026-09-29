//! Error type shared by the parsers and encoders of this crate.

/// Everything that can go wrong while parsing or building data-application structures.
///
/// Rust note: `#[derive(thiserror::Error)]` generates the `std::error::Error` and
/// `Display` impls from the `#[error("...")]` attributes, so callers can use `?` and
/// print these errors like any other.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataError {
    /// The input ended before a field that the header said was there.
    #[error("data truncated")]
    Truncated,
    /// A CRC did not match the data it protects.
    #[error("CRC check failed")]
    CrcMismatch,
    /// A structure was syntactically invalid (the payload names the structure).
    #[error("malformed {0}")]
    Malformed(&'static str),
    /// A value does not fit the field that has to carry it.
    #[error("{0} out of range")]
    OutOfRange(&'static str),
    /// gzip / deflate decompression failed or exceeded the size limit.
    #[error("decompression failed")]
    Decompress,
    /// A valid but unsupported feature (for example an unknown compression scheme).
    #[error("unsupported {0}")]
    Unsupported(&'static str),
    /// An invalid [`crate::DataServiceConfig`].
    #[error("invalid configuration: {0}")]
    Config(&'static str),
    /// An EPG attribute value that cannot be encoded in binary form.
    #[error("invalid EPG value for attribute `{attribute}`: {value}")]
    EpgValue {
        /// Attribute name.
        attribute: String,
        /// The offending value (as text).
        value: String,
    },
}

/// Shorthand used throughout the crate.
pub type Result<T> = std::result::Result<T, DataError>;
