//! Basic DRM30 transmission parameters: robustness modes, spectrum occupancy and the
//! OFDM numerology derived from them (ES 201 980 §8.1).
//!
//! DecDRM always processes the signal at [`SAMPLE_RATE`] (48 kHz), at which every
//! robustness mode has an integer number of samples per useful symbol part and guard
//! interval (the same choice Dream makes).

use std::fmt;

/// Working sample rate of the receiver and transmitter chains, in Hz.
pub const SAMPLE_RATE: u32 = 48_000;

/// Transmission frames per transmission super frame (§8.1).
pub const FRAMES_PER_SUPERFRAME: usize = 3;

/// Duration of one transmission frame in samples at [`SAMPLE_RATE`] (400 ms).
pub const SAMPLES_PER_FRAME: usize = 19_200;

/// Maximum number of streams in the MSC multiplex (§6.2.1).
pub const MAX_STREAMS: usize = 4;

/// Maximum number of services (§6.3).
pub const MAX_SERVICES: usize = 4;

/// DRM30 robustness mode (§8.1, table 82).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RobustnessMode {
    A,
    B,
    C,
    D,
}

impl RobustnessMode {
    /// All DRM30 modes, in order.
    pub const ALL: [RobustnessMode; 4] = [Self::A, Self::B, Self::C, Self::D];

    /// Index 0..=3, used to address per-mode tables.
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Decode the 2-bit robustness mode field used in SDC/MDI signalling.
    pub const fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::A),
            1 => Some(Self::B),
            2 => Some(Self::C),
            3 => Some(Self::D),
            _ => None,
        }
    }

    /// Length of the useful symbol part Tu in samples at 48 kHz (= FFT size).
    pub const fn fft_size(self) -> usize {
        match self {
            Self::A => 1152,
            Self::B => 1024,
            Self::C => 704,
            Self::D => 448,
        }
    }

    /// Ratio Tg/Tu as (numerator, denominator).
    pub const fn guard_ratio(self) -> (usize, usize) {
        match self {
            Self::A => (1, 9),
            Self::B => (1, 4),
            Self::C => (4, 11),
            Self::D => (11, 14),
        }
    }

    /// Guard interval length Tg in samples at 48 kHz.
    pub const fn guard_len(self) -> usize {
        let (num, den) = self.guard_ratio();
        self.fft_size() * num / den
    }

    /// Total symbol length Ts = Tu + Tg in samples at 48 kHz.
    pub const fn symbol_len(self) -> usize {
        self.fft_size() + self.guard_len()
    }

    /// Number of OFDM symbols per transmission frame Ns.
    pub const fn symbols_per_frame(self) -> usize {
        match self {
            Self::A | Self::B => 15,
            Self::C => 20,
            Self::D => 24,
        }
    }

    /// Number of OFDM symbols per transmission super frame.
    pub const fn symbols_per_superframe(self) -> usize {
        self.symbols_per_frame() * FRAMES_PER_SUPERFRAME
    }

    /// Carrier spacing 1/Tu in Hz.
    pub fn carrier_spacing(self) -> f64 {
        f64::from(SAMPLE_RATE) / self.fft_size() as f64
    }

    /// Number of OFDM symbols at the start of each super frame that carry the SDC.
    pub const fn sdc_symbols(self) -> usize {
        match self {
            Self::A | Self::B => 2,
            Self::C | Self::D => 3,
        }
    }
}

impl fmt::Display for RobustnessMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = match self {
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
            Self::D => 'D',
        };
        write!(f, "{c}")
    }
}

/// Spectrum occupancy 0..=5 (§8.1, table 83): nominal channel bandwidths of
/// 4.5, 5, 9, 10, 18 and 20 kHz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpectrumOccupancy(u8);

impl SpectrumOccupancy {
    pub const SO_0: Self = Self(0);
    pub const SO_1: Self = Self(1);
    pub const SO_2: Self = Self(2);
    pub const SO_3: Self = Self(3);
    pub const SO_4: Self = Self(4);
    pub const SO_5: Self = Self(5);

    pub const ALL: [SpectrumOccupancy; 6] =
        [Self::SO_0, Self::SO_1, Self::SO_2, Self::SO_3, Self::SO_4, Self::SO_5];

    /// Values 0..=5 are valid; the FAC field is 3 bits so 6 and 7 are rejected.
    pub const fn new(value: u8) -> Option<Self> {
        if value <= 5 { Some(Self(value)) } else { None }
    }

    pub const fn value(self) -> u8 {
        self.0
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Nominal channel bandwidth in kHz.
    pub const fn bandwidth_khz(self) -> f64 {
        match self.0 {
            0 => 4.5,
            1 => 5.0,
            2 => 9.0,
            3 => 10.0,
            4 => 18.0,
            _ => 20.0,
        }
    }
}

impl fmt::Display for SpectrumOccupancy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SO{} ({} kHz)", self.0, self.bandwidth_khz())
    }
}

/// Lowest and highest carrier index (Kmin, Kmax) for a mode/occupancy pair
/// (§8.1, table 84). Returns `None` for combinations the standard does not define
/// (modes C and D only exist with occupancies 3 and 5).
pub const fn carrier_range(mode: RobustnessMode, so: SpectrumOccupancy) -> Option<(i32, i32)> {
    const KMIN: [[i32; 4]; 6] = [
        [2, 1, 0, 0],
        [2, 1, 0, 0],
        [-102, -91, 0, 0],
        [-114, -103, -69, -44],
        [-98, -87, 0, 0],
        [-110, -99, -67, -43],
    ];
    const KMAX: [[i32; 4]; 6] = [
        [102, 91, 0, 0],
        [114, 103, 0, 0],
        [102, 91, 0, 0],
        [114, 103, 69, 44],
        [314, 279, 0, 0],
        [350, 311, 213, 135],
    ];
    let kmin = KMIN[so.index()][mode.index()];
    let kmax = KMAX[so.index()][mode.index()];
    if kmin == 0 && kmax == 0 { None } else { Some((kmin, kmax)) }
}

/// A valid (mode, occupancy) combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelLayout {
    pub mode: RobustnessMode,
    pub occupancy: SpectrumOccupancy,
}

impl ChannelLayout {
    pub fn new(mode: RobustnessMode, occupancy: SpectrumOccupancy) -> Option<Self> {
        carrier_range(mode, occupancy).map(|_| Self { mode, occupancy })
    }

    /// (Kmin, Kmax) — always defined for a constructed layout.
    pub fn carrier_range(self) -> (i32, i32) {
        carrier_range(self.mode, self.occupancy).expect("validated in ChannelLayout::new")
    }

    /// Number of carriers from Kmin to Kmax inclusive (including unused DC carriers).
    pub fn num_carriers(self) -> usize {
        let (kmin, kmax) = self.carrier_range();
        (kmax - kmin + 1) as usize
    }
}

impl fmt::Display for ChannelLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mode {} / {}", self.mode, self.occupancy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_fills_exactly_400_ms() {
        for m in RobustnessMode::ALL {
            assert_eq!(m.symbol_len() * m.symbols_per_frame(), SAMPLES_PER_FRAME, "mode {m}");
        }
    }

    #[test]
    fn guard_lengths_match_spec() {
        let g: Vec<_> = RobustnessMode::ALL.iter().map(|m| m.guard_len()).collect();
        assert_eq!(g, [128, 256, 256, 352]);
    }

    #[test]
    fn modes_c_and_d_only_have_so3_and_so5() {
        for so in SpectrumOccupancy::ALL {
            let defined = matches!(so.value(), 3 | 5);
            assert_eq!(ChannelLayout::new(RobustnessMode::C, so).is_some(), defined);
            assert_eq!(ChannelLayout::new(RobustnessMode::D, so).is_some(), defined);
            assert!(ChannelLayout::new(RobustnessMode::A, so).is_some());
            assert!(ChannelLayout::new(RobustnessMode::B, so).is_some());
        }
    }
}
