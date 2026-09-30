//! EVS frame signalling at 13.2 kbit/s (3GPP TS 26.445 §7.1; reference code
//! `acelp_sig_tbl`, lib_com/rom_com.c, read by `decision_matrix_core_dec`): the first
//! 5 bits of a primary-mode frame select the coder type, audio bandwidth and flags of
//! an ACELP frame, or the low-rate MDCT core.

/// Audio bandwidth of an EVS frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bandwidth {
    /// Narrowband, 4 kHz.
    Nb,
    /// Wideband, 8 kHz.
    Wb,
    /// Super-wideband, 14 kHz.
    Swb,
    /// Fullband, 20 kHz.
    Fb,
}

impl Bandwidth {
    /// "NB", "WB", "SWB" or "FB".
    pub fn name(self) -> &'static str {
        match self {
            Bandwidth::Nb => "NB",
            Bandwidth::Wb => "WB",
            Bandwidth::Swb => "SWB",
            Bandwidth::Fb => "FB",
        }
    }

    /// The codec's internal sampling rate for this bandwidth, Hz.
    pub fn sample_rate_hz(self) -> u32 {
        match self {
            Bandwidth::Nb => 8_000,
            Bandwidth::Wb => 16_000,
            Bandwidth::Swb => 32_000,
            Bandwidth::Fb => 48_000,
        }
    }
}

/// Coder type of an EVS frame (lib_com/cnst.h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoderType {
    Inactive,
    Unvoiced,
    Voiced,
    Generic,
    Transition,
    /// Generic signal coding (music).
    Audio,
    /// The low-rate MDCT core instead of ACELP.
    LowRateMdct,
}

/// What the first bits of a frame signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signalling {
    pub coder_type: CoderType,
    pub bandwidth: Bandwidth,
    /// Formant sharpening flag.
    pub sharpening: bool,
    /// Channel-aware mode.
    pub channel_aware: bool,
}

const fn s(coder_type: CoderType, bandwidth: Bandwidth, sharpening: bool, channel_aware: bool) -> Signalling {
    Signalling { coder_type, bandwidth, sharpening, channel_aware }
}

use Bandwidth::{Nb, Swb, Wb};
use CoderType::{Audio, Generic, Inactive, LowRateMdct, Transition, Unvoiced, Voiced};

/// `acelp_sig_tbl`, the ACELP_13k20 section (5 bits, 32 entries).
const TABLE_13K2: [Signalling; 32] = [
    s(Generic, Nb, true, false),
    s(Voiced, Nb, true, false),
    s(Transition, Nb, false, false),
    s(Audio, Nb, false, false),
    s(Inactive, Nb, false, false),
    s(Generic, Wb, true, false),
    s(Voiced, Wb, true, false),
    s(Transition, Wb, false, false),
    s(Audio, Wb, false, false),
    s(Inactive, Wb, false, false),
    s(Generic, Swb, true, false),
    s(Voiced, Swb, true, false),
    s(Transition, Swb, false, false),
    s(Audio, Swb, false, false),
    s(Inactive, Swb, false, false),
    s(Generic, Nb, false, false),
    s(Voiced, Nb, false, false),
    s(Generic, Wb, false, false),
    s(Voiced, Wb, false, false),
    s(Generic, Swb, false, false),
    s(Voiced, Swb, false, false),
    s(Generic, Wb, true, true),
    s(Unvoiced, Wb, false, true),
    s(Voiced, Wb, true, true),
    s(Inactive, Wb, false, true),
    s(Generic, Swb, true, true),
    s(Unvoiced, Swb, false, true),
    s(Voiced, Swb, true, true),
    s(Inactive, Swb, false, true),
    s(LowRateMdct, Nb, false, false),
    s(LowRateMdct, Wb, false, false),
    s(LowRateMdct, Swb, false, false),
];

/// Bits of a 13.2 kbit/s frame (20 ms).
pub const BITS_13K2: usize = 264;

/// The signalling of a 13.2 kbit/s frame from its first byte.
pub fn signalling_13k2(first_byte: u8) -> Signalling {
    TABLE_13K2[usize::from(first_byte >> 3)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame types seen from KCBS on 6140 kHz: all super-wideband ACELP.
    #[test]
    fn kcbs_markers() {
        let t = |b: u8| {
            let s = signalling_13k2(b);
            (s.coder_type, s.bandwidth)
        };
        assert_eq!(t(0x58), (Voiced, Swb));
        assert_eq!(t(0x53), (Generic, Swb));
        assert_eq!(t(0x61), (Transition, Swb));
        assert_eq!(t(0x76), (Inactive, Swb));
        assert_eq!(t(0x9b), (Generic, Swb));
        assert!(!signalling_13k2(0x9b).sharpening && signalling_13k2(0x58).sharpening);
        assert_eq!(t(0xff), (LowRateMdct, Swb));
        assert_eq!(t(0x00), (Generic, Nb));
    }
}
