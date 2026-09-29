//! Conversion of raw sound-card / file samples into the complex 48 kHz stream the
//! receiver works on (the role of Dream's `CReceiveData`).
//!
//! Real inputs (a receiver's IF or audio output) are turned into their analytic
//! signal, so everything downstream can treat real and I/Q inputs alike: the DRM
//! signal simply sits somewhere in the complex spectrum.

use crate::dsp::fir::AnalyticConverter;
use crate::{Cplx, Real};

/// Which channel(s) of a real input carry the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RealChannel {
    #[default]
    Left,
    Right,
    /// (L + R) / 2 — also right for mono sources.
    Mix,
    /// (L − R) / 2.
    Diff,
}

/// How the input samples represent the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    /// Real-valued signal (IF / audio), taken from `channel`.
    Real(RealChannel),
    /// Complex baseband / low-IF I/Q: I on the left channel and Q on the right
    /// (`swap` exchanges them, which mirrors the spectrum).
    Iq { swap: bool },
}

impl Default for InputFormat {
    fn default() -> Self {
        Self::Real(RealChannel::Mix)
    }
}

/// Streaming converter from interleaved `f32` frames to complex samples.
#[derive(Debug, Clone)]
pub struct InputConverter {
    format: InputFormat,
    channels: usize,
    flip: bool,
    analytic: AnalyticConverter,
    real_buf: Vec<Real>,
    sign: Real,
}

impl InputConverter {
    /// `channels` is the number of interleaved channels of the source (1 or 2).
    /// `flip` mirrors the spectrum (e.g. LSB reception of an upper-sideband signal).
    pub fn new(format: InputFormat, channels: usize, flip: bool) -> Self {
        Self {
            format,
            channels: channels.max(1),
            flip,
            analytic: AnalyticConverter::new(127),
            real_buf: Vec::new(),
            sign: 1.0,
        }
    }

    pub fn format(&self) -> InputFormat {
        self.format
    }

    pub fn flipped(&self) -> bool {
        self.flip
    }

    /// Toggle spectrum inversion at runtime (used by automatic flip detection).
    pub fn set_flip(&mut self, flip: bool) {
        self.flip = flip;
    }

    /// Convert interleaved frames into complex samples appended to `out`.
    pub fn process(&mut self, interleaved: &[f32], out: &mut Vec<Cplx>) {
        let ch = self.channels;
        let frames = interleaved.len() / ch;
        let sample = |f: usize, c: usize| -> Real {
            if c < ch { Real::from(interleaved[f * ch + c]) } else { Real::from(interleaved[f * ch]) }
        };
        match self.format {
            InputFormat::Real(sel) => {
                self.real_buf.clear();
                for f in 0..frames {
                    let v = match sel {
                        RealChannel::Left => sample(f, 0),
                        RealChannel::Right => sample(f, 1),
                        RealChannel::Mix => {
                            if ch == 1 {
                                sample(f, 0)
                            } else {
                                0.5 * (sample(f, 0) + sample(f, 1))
                            }
                        }
                        RealChannel::Diff => 0.5 * (sample(f, 0) - sample(f, 1)),
                    };
                    // Mirroring a real spectrum = shifting it by fs/2, i.e. negating
                    // every other sample (as Dream does).
                    let v = if self.flip {
                        self.sign = -self.sign;
                        v * self.sign
                    } else {
                        v
                    };
                    self.real_buf.push(v);
                }
                self.analytic.process(&self.real_buf, out);
            }
            InputFormat::Iq { swap } => {
                for f in 0..frames {
                    let (i, q) = if swap { (sample(f, 1), sample(f, 0)) } else { (sample(f, 0), sample(f, 1)) };
                    let c = Cplx::new(i, q);
                    out.push(if self.flip { c.conj() } else { c });
                }
            }
        }
    }

    /// True if the signal can only be at positive frequencies (real input).
    pub fn is_real(&self) -> bool {
        matches!(self.format, InputFormat::Real(_))
    }
}
