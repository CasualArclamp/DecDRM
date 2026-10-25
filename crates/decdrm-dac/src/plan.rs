//! Choosing the bandwidth and framing that fit an audio stream.
//!
//! The multiplex plan gives the audio stream whatever capacity is left, so the audio
//! super frame length is an input. The policy, for a super frame of `len` bytes:
//!
//! 1. **Bandwidth.** The largest tier whose codes and CRCs fit with CRC groups of at
//!    most 6 frames (80 ms, the length of an HE-AAC frame at a 12 kHz core). Only if
//!    even 1.5 kbit/s does not fit that way, or a requested bandwidth does not, coarser
//!    groups (10, 15, 30 frames) are allowed.
//! 2. **Repetition before granularity.** The spare bytes then buy robustness: first as
//!    many repeated layers as fit (a second copy turns a region loss probability *p*
//!    into roughly *p*², far more than finer CRC groups gain), then the finest group
//!    size (3, 5 or 6 frames; never finer than 40 ms — the decoder network smears errors
//!    over neighbouring frames anyway, and the CRC overhead would grow past 20 %).
//!
//! Whatever is still left over is zero padding.
//!
//! **Joint stereo** ([`choose_stereo_config`]): the side signal gets a fixed share, by
//! default an eighth of the mid signal's bit rate and at least 1.5 kbit/s
//! ([`default_side`]: 12 kbit/s mid + 1.5 side, 24 + 3). The mid bandwidth is the
//! largest for which both fit as above; the spare bytes go to the mid signal's framing
//! first (a lost mid frame is a gap in the sound, a lost side frame only narrows the
//! stereo image for a moment), then to the side's.

use crate::config::{Bandwidth, DacConfig, MAX_REPEATED_LAYERS};
use crate::framing::FrameLayout;

/// Largest CRC group (frames) the automatic bandwidth choice accepts: 80 ms.
pub const ADMISSION_GROUP_FRAMES: usize = 6;
/// Group sizes tried when the admission group fits (finest first).
const FINE_GROUPS: [usize; 3] = [3, 5, 6];
/// Group sizes for streams that only fit with coarser groups.
const COARSE_GROUPS: [usize; 3] = [10, 15, 30];

/// The stream is too small for DAC (at the requested bandwidth).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanError {
    /// The (lowest) bandwidth that was tried (of the mid signal for joint stereo).
    pub bandwidth: Bandwidth,
    /// Joint stereo: the side signal's bandwidth that was tried with it.
    pub side: Option<Bandwidth>,
    /// Bytes per 400 ms that bandwidth needs at least (with 400 ms CRC groups).
    pub needed: usize,
    /// Bytes available.
    pub available: usize,
}

impl PlanError {
    /// The rate that did not fit: `6 kbit/s`, or `6 kbit/s + 1.5 kbit/s side`.
    pub fn rates(&self) -> String {
        match self.side {
            None => self.bandwidth.to_string(),
            Some(s) => format!("{} + {s} side", self.bandwidth),
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "DAC {} needs an audio super frame of at least {} bytes ({:.2} kbit/s), the stream leaves {}",
            self.rates(),
            self.needed,
            self.needed as f64 * 8.0 / 400.0,
            self.available
        )
    }
}

impl std::error::Error for PlanError {}

/// Bytes a configuration needs.
pub fn required_bytes(bandwidth: Bandwidth, group_frames: usize, repeated_layers: usize) -> usize {
    DacConfig::new(bandwidth, group_frames, repeated_layers).map_or(usize::MAX, |c| FrameLayout::new(c).min_bytes())
}

/// The configuration for an audio super frame (stream minus text message bytes) of
/// `len` bytes: the requested bandwidth, or the largest that fits (see the module
/// docs).
pub fn choose_config(len: usize, requested: Option<Bandwidth>) -> Result<DacConfig, PlanError> {
    let candidates: Vec<Bandwidth> = match requested {
        Some(b) => vec![b],
        None => Bandwidth::ALL.iter().rev().copied().collect(),
    };
    for (admission, groups) in [(ADMISSION_GROUP_FRAMES, FINE_GROUPS), (30, COARSE_GROUPS)] {
        if let Some(&bw) = candidates.iter().find(|&&bw| required_bytes(bw, admission, 0) <= len) {
            return Ok(best_framing(bw, &groups, len));
        }
    }
    let lowest = *candidates.last().expect("at least one candidate");
    Err(PlanError { bandwidth: lowest, side: None, needed: required_bytes(lowest, 30, 0), available: len })
}

/// The side signal's default bandwidth for a mid bandwidth: an eighth of its bit rate,
/// at least 1.5 kbit/s. On the stereo music measured (`docs/DESIGN.md`), 2 side
/// codebooks took the left/right log-spectral distance from 5.2 dB (mono) to 2.8 dB
/// next to a 12 kbit/s mid signal, 4 to 2.3, 8 to 1.8.
pub fn default_side(mid: Bandwidth) -> Bandwidth {
    Bandwidth::from_tier(mid.tier().saturating_sub(3)).expect("tier 0..=4")
}

/// The joint stereo configuration for an audio super frame of `len` bytes: the
/// requested mid bandwidth, or the largest that fits next to its side signal; the
/// requested side bandwidth, or [`default_side`] (see the module docs).
pub fn choose_stereo_config(
    len: usize,
    requested: Option<Bandwidth>,
    requested_side: Option<Bandwidth>,
) -> Result<DacConfig, PlanError> {
    let candidates: Vec<(Bandwidth, Bandwidth)> = match requested {
        Some(b) => vec![b],
        None => Bandwidth::ALL.iter().rev().copied().collect(),
    }
    .into_iter()
    .map(|mid| (mid, requested_side.unwrap_or_else(|| default_side(mid))))
    .collect();
    let need = |(mid, side): (Bandwidth, Bandwidth), group: usize| {
        required_bytes(mid, group, 0).saturating_add(required_bytes(side, group, 0))
    };
    for (admission, groups) in [(ADMISSION_GROUP_FRAMES, FINE_GROUPS), (30, COARSE_GROUPS)] {
        if let Some(&(mid, side)) = candidates.iter().find(|&&c| need(c, admission) <= len) {
            // The mid signal first, leaving the side its smallest framing.
            let side_min = required_bytes(side, admission, 0);
            let mid = best_framing(mid, &groups, len - side_min);
            let side = best_framing(side, &groups, len - FrameLayout::new(mid).min_bytes());
            return Ok(DacConfig::joint_stereo(mid, side));
        }
    }
    let lowest = *candidates.last().expect("at least one candidate");
    Err(PlanError { bandwidth: lowest.0, side: Some(lowest.1), needed: need(lowest, 30), available: len })
}

/// Most repeated layers, then the finest group, that fit into `len` bytes. The largest
/// group without repetition must fit.
fn best_framing(bw: Bandwidth, groups: &[usize], len: usize) -> DacConfig {
    for r in (0..=MAX_REPEATED_LAYERS.min(bw.layers())).rev() {
        if let Some(&g) = groups.iter().find(|&&g| required_bytes(bw, g, r) <= len) {
            return DacConfig::new(bw, g, r).expect("valid by construction");
        }
    }
    unreachable!("the caller checked that {bw} fits with {} frames per group", groups[groups.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(len: usize) -> (f64, usize, usize) {
        let c = choose_config(len, None).unwrap();
        (c.bandwidth.kbps(), c.group_frames, c.repeated_layers)
    }

    /// Audio super frame lengths of typical channels (stream bytes of a single audio
    /// service, from `decdrm tx --check`).
    #[test]
    fn typical_channels() {
        // Mode D, 10 kHz, 16-QAM, protection 0: 3 kbit/s with the base layer twice.
        assert_eq!(plan(304), (3.0, 3, 1));
        // Mode D, 10 kHz, 16-QAM, protection 1: 6 kbit/s, 40 ms groups.
        assert_eq!(plan(381), (6.0, 3, 0));
        // Mode D, 10 kHz, 64-QAM, protection 1: 6 kbit/s with 3 kbit/s repeated.
        assert_eq!(plan(548), (6.0, 3, 2));
        // Mode B, 10 kHz, 16-QAM, protection 0: 6 kbit/s with 3 kbit/s repeated.
        assert_eq!(plan(582), (6.0, 3, 2));
        // Mode B, 10 kHz, 64-QAM, protection 1: 12 kbit/s with 6 kbit/s repeated.
        assert_eq!(plan(1048), (12.0, 3, 3));
        // Mode A, 10 kHz, 64-QAM, protection 3: 24 kbit/s with 6 kbit/s repeated.
        assert_eq!(plan(1738), (24.0, 3, 3));
        // Mode B, 4.5 kHz, 16-QAM, protection 0 (240 bytes): 3 kbit/s; the base layer
        // copy only fits with 80 ms groups, and repetition wins.
        assert_eq!(plan(240), (3.0, 6, 1));
    }

    #[test]
    fn edges() {
        // 1.5 kbit/s with 80 ms groups needs 75 + 5 bytes; with 400 ms groups 76.
        assert_eq!(plan(80), (1.5, 6, 0));
        assert_eq!(plan(76), (1.5, 30, 0));
        let e = choose_config(75, None).unwrap_err();
        assert_eq!((e.bandwidth, e.needed, e.available), (Bandwidth::Kbps1_5, 76, 75));
        assert!(e.to_string().contains("1.5 kbit/s"), "{e}");
        // A requested bandwidth uses coarse groups rather than failing: 6 kbit/s in
        // 304 bytes needs 400 ms groups (303 bytes).
        let c = choose_config(304, Some(Bandwidth::Kbps6)).unwrap();
        assert_eq!((c.group_frames, c.repeated_layers), (30, 0));
        let e = choose_config(302, Some(Bandwidth::Kbps6)).unwrap_err();
        assert_eq!((e.needed, e.bandwidth), (303, Bandwidth::Kbps6));
        // Everything chosen fits, and more room never lowers the bandwidth.
        let mut last = Bandwidth::Kbps1_5;
        for len in 76..3000 {
            let c = choose_config(len, None).unwrap();
            assert!(FrameLayout::new(c).min_bytes() <= len);
            assert!(c.bandwidth >= last, "{len}");
            last = c.bandwidth;
        }
    }

    /// Joint stereo: mid + side, the spare bytes on the mid signal first.
    #[test]
    fn joint_stereo() {
        let plan = |len: usize| {
            let c = choose_stereo_config(len, None, None).unwrap();
            let s = c.side.unwrap();
            assert!(c.min_bytes() <= len);
            (c.bandwidth.kbps(), c.group_frames, c.repeated_layers, s.bandwidth.kbps(), s.group_frames, s.repeated_layers)
        };
        assert_eq!([12.0, 24.0, 6.0, 3.0, 1.5].map(|k| default_side(Bandwidth::from_kbps(k).unwrap()).kbps()), [
            1.5, 3.0, 1.5, 1.5, 1.5
        ]);
        // Mode B, 10 kHz, 64-QAM, protection 1: 12 kbit/s with 6 kbit/s repeated in
        // 67 ms groups, 1.5 side.
        assert_eq!(plan(1048), (12.0, 5, 3, 1.5, 3, 0));
        // Mode B, 10 kHz, 16-QAM, protection 0: 6 kbit/s with 3 repeated, 1.5 side.
        assert_eq!(plan(582), (6.0, 3, 2, 1.5, 5, 0));
        // Mode A, 10 kHz, 64-QAM, protection 3: 24 + 3.
        assert_eq!(plan(1738).0, 24.0);
        assert_eq!(plan(1738).3, 3.0);
        // Requested rates.
        let c = choose_stereo_config(1048, Some(Bandwidth::Kbps6), Some(Bandwidth::Kbps3)).unwrap();
        assert_eq!((c.bandwidth, c.side.unwrap().bandwidth), (Bandwidth::Kbps6, Bandwidth::Kbps3));
        let e = choose_stereo_config(381, Some(Bandwidth::Kbps12), None).unwrap_err();
        assert_eq!(e.rates(), "12 kbit/s + 1.5 kbit/s side");
        assert!(e.to_string().starts_with("DAC 12 kbit/s + 1.5 kbit/s side needs"), "{e}");
        let e = choose_stereo_config(150, None, None).unwrap_err();
        assert_eq!((e.bandwidth, e.side, e.needed), (Bandwidth::Kbps1_5, Some(Bandwidth::Kbps1_5), 152));
        // More room never lowers the mid bandwidth.
        let mut last = Bandwidth::Kbps1_5;
        for len in 152..3000 {
            let c = choose_stereo_config(len, None, None).unwrap();
            assert!(c.min_bytes() <= len, "{len}");
            assert!(c.bandwidth >= last, "{len}");
            last = c.bandwidth;
        }
    }
}
