//! DecDRM core: the DRM30 (ETSI ES 201 980) physical layer, channel coding,
//! multiplex handling and the transmitter chain, in pure Rust.
//!
//! All signal processing runs in `f64` ([`Real`]/[`Cplx`]); see `docs/DESIGN.md`.

// Index loops over parallel arrays (carriers, taps, pilots) read like the formulas
// in the specification; iterator chains would obscure them.
#![allow(clippy::needless_range_loop)]

pub mod bits;
pub mod cellmap;
pub mod channel;
pub mod dsp;
pub mod fac;
pub mod fec;
pub mod interleave;
pub mod mux;
pub mod params;
pub mod rx;
pub mod tables;
pub mod tx;

/// Real sample type used throughout the DSP chain.
pub type Real = f64;
/// Complex sample type used throughout the DSP chain.
pub type Cplx = num_complex::Complex<f64>;
