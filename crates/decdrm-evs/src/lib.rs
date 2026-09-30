//! 3GPP EVS (Enhanced Voice Services) audio for DecDRM.
//!
//! EVS is not a DRM audio coding, but Korean Central Broadcasting (KCBS, 6140 kHz)
//! sends EVS at 13.2 kbit/s inside a data service ([`kcbs`] describes the framing).
//! This crate recognises and cuts out those frames ([`kcbs`], [`signalling`]); with the
//! `decoder` feature it also decodes them ([`EvsDecoder`]) with the 3GPP floating-point
//! reference decoder (TS 26.443).
//!
//! The reference code is not part of DecDRM: it is © 3GPP Organizational Partners and
//! EVS is patent-licensed, so it is for private use. The build compiles it from the
//! 3GPP or ETSI zip placed in `reference/evs/` (see build.rs); without it, and without
//! the feature, EVS services are recognised and reported as not decodable.

pub mod kcbs;
pub mod signalling;

#[cfg(feature = "decoder")]
mod decoder;
#[cfg(feature = "decoder")]
pub use decoder::{EvsDecoder, EvsError};

/// Whether the EVS decoder is built in (the `decoder` feature).
pub const BUILT_IN: bool = cfg!(feature = "decoder");
