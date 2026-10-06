//! Smoothing of the SBR band of decoded HE-AAC / xHE-AAC audio.
//!
//! Some xHE-AAC encoders switch the SBR band (above the core coder's bandwidth) on and off from
//! one frame to the next. CNR-1 on 13790/13835 kHz sends SBR envelopes whose level jumps by
//! more than 18 dB between consecutive 64 ms frames about once a second, which is heard as
//! glitching. A correct decoder reproduces that; [`HighBandSmoother`] is an optional
//! post-filter that limits how fast the band's level may change:
//!
//! 1. A linear-phase FIR low-pass (windowed sinc, Blackman window, ≈800 Hz transition) splits
//!    off the band below the crossover. The high band is the delayed input minus it, so the
//!    two add up to the delayed input exactly.
//! 2. The high band's power is measured in 16 ms blocks (all channels together).
//! 3. A block's output level is at most its own level, the previous block's output level plus
//!    [`SLEW_DB`], and each of the next [`LOOKAHEAD_BLOCKS`] blocks' levels plus `SLEW_DB` per
//!    block of distance. So the level rises and falls by at most `SLEW_DB` per block, fading
//!    out before a drop, and is never raised: nothing is invented in the gaps.
//! 4. The gains are applied to the high band, interpolated linearly from each block's centre to
//!    the smaller of the neighbouring gains at its edges.
//!
//! Measured on a 13835 kHz recording (crossover 6 kHz): high-band level jumps of more than
//! 20 dB between 16 ms blocks fall from 53 to about 2 a minute, and the band loses ≈10 dB on
//! average. Finer blocks (8 or 4 ms) did worse: their noisier levels pull the gain down more,
//! and on a synthetic band gated by 40 dB they let larger steps through. The output is the input delayed by [`HighBandSmoother::latency`] frames (≈100 ms,
//! zeros at the start), as many frames per call as went in.

use std::collections::VecDeque;

/// Block length for the high band's level, seconds.
const BLOCK_S: f64 = 0.016;
/// Transition width of the crossover low-pass, Hz.
const TRANSITION_HZ: f64 = 800.0;
/// Power floor (−120 dB) so that a silent block has a finite level.
const POWER_FLOOR: f64 = 1e-12;
/// Largest change of the high band's level from one 16 ms block to the next, dB.
pub const SLEW_DB: f64 = 3.0;
/// Blocks the gain looks ahead, so that the band fades out before a drop.
pub const LOOKAHEAD_BLOCKS: usize = 4;

/// Streaming SBR band smoother for interleaved `f32` audio (see the module docs).
#[derive(Debug, Clone)]
pub struct HighBandSmoother {
    sample_rate: u32,
    channels: usize,
    crossover_hz: f64,
    /// Linear-phase low-pass, odd length.
    taps: Vec<f32>,
    /// Per channel: the last `taps.len() - 1` input samples (zeros at the start).
    history: Vec<VecDeque<f32>>,
    /// Block length in frames.
    block: usize,
    /// Low and high band frames (interleaved) not yet output; the front starts a block.
    low: VecDeque<f32>,
    high: VecDeque<f32>,
    /// High-band power summed over the frames of the block being filled.
    acc: f64,
    acc_frames: usize,
    /// Levels (dB) of measured blocks whose gain is not computed yet, oldest first.
    levels: VecDeque<f64>,
    /// Output level of the last block whose gain was computed.
    last_out: Option<f64>,
    /// Linear gains of the block before the next one to output (front), then the following
    /// blocks with a computed gain.
    gains: VecDeque<f64>,
    /// Gains have started (the first output block has no predecessor).
    started: bool,
    /// Output frames ready (interleaved), led by `latency` frames of zeros.
    out: VecDeque<f32>,
}

impl HighBandSmoother {
    /// A smoother for `channels` interleaved channels at `sample_rate` Hz, splitting at
    /// `crossover_hz`; `None` if the crossover is not inside (0, 0.45 · sample rate).
    pub fn new(sample_rate: u32, channels: usize, crossover_hz: f64) -> Option<Self> {
        let fs = f64::from(sample_rate);
        if channels == 0 || !(crossover_hz > 0.0 && crossover_hz < 0.45 * fs) {
            return None;
        }
        // Blackman window: transition ≈ 5.5 · fs / N.
        let n = ((5.5 * fs / TRANSITION_HZ).round() as usize) | 1;
        let mid = (n / 2) as f64;
        let fc = crossover_hz / fs;
        let mut taps: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 - mid;
                let sinc = if t == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t) };
                let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
                sinc * w
            })
            .collect();
        let sum: f64 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        let block = ((fs * BLOCK_S).round() as usize).max(1);
        let latency = (LOOKAHEAD_BLOCKS + 2) * block;
        Some(Self {
            sample_rate,
            channels,
            crossover_hz,
            taps: taps.into_iter().map(|t| t as f32).collect(),
            history: vec![VecDeque::from(vec![0.0; n - 1]); channels],
            block,
            low: VecDeque::new(),
            high: VecDeque::new(),
            acc: 0.0,
            acc_frames: 0,
            levels: VecDeque::new(),
            last_out: None,
            gains: VecDeque::new(),
            started: false,
            out: VecDeque::from(vec![0.0; latency * channels]),
        })
    }

    /// Sample rate this smoother was made for, Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Channels this smoother was made for.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The crossover, Hz.
    pub fn crossover_hz(&self) -> f64 {
        self.crossover_hz
    }

    /// Delay from input to output, frames: the crossover filter's group delay plus the
    /// look-ahead and the interpolation (`LOOKAHEAD_BLOCKS + 2` blocks).
    pub fn latency(&self) -> usize {
        (self.taps.len() - 1) / 2 + (LOOKAHEAD_BLOCKS + 2) * self.block
    }

    /// Process interleaved frames; returns as many frames, delayed by [`Self::latency`].
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let ch = self.channels;
        let frames = input.len() / ch;
        let delay = (self.taps.len() - 1) / 2;
        // 1. Split: low = FIR(x), high = x delayed by the filter's group delay − low.
        for f in 0..frames {
            let mut power = 0.0;
            for c in 0..ch {
                let h = &mut self.history[c];
                h.push_back(input[f * ch + c]);
                // `h` now holds the last `taps.len()` samples, oldest first; the taps are
                // symmetric, so their order does not matter.
                let lo: f32 = h.iter().zip(&self.taps).map(|(x, t)| x * t).sum();
                let hi = h[h.len() - 1 - delay] - lo;
                h.pop_front();
                self.low.push_back(lo);
                self.high.push_back(hi);
                power += f64::from(hi) * f64::from(hi);
            }
            // 2. Block levels.
            self.acc += power;
            self.acc_frames += 1;
            if self.acc_frames == self.block {
                self.levels.push_back(10.0 * (self.acc / self.block as f64 + POWER_FLOOR).log10());
                self.acc = 0.0;
                self.acc_frames = 0;
            }
        }
        // 3. Gains, once a block's look-ahead is measured.
        while self.levels.len() > LOOKAHEAD_BLOCKS {
            let own = self.levels[0];
            let mut level = own;
            if let Some(prev) = self.last_out {
                level = level.min(prev + SLEW_DB);
            }
            for (j, next) in self.levels.iter().enumerate().skip(1) {
                level = level.min(next + SLEW_DB * j as f64);
            }
            self.last_out = Some(level);
            let gain = 10f64.powf((level - own) / 20.0);
            if !self.started {
                self.gains.push_back(gain); // the first block's "previous" gain
                self.started = true;
            }
            self.gains.push_back(gain);
            self.levels.pop_front();
        }
        // 4. Output whole blocks whose next block's gain is known: gains[0] is the previous
        //    block's, gains[1] this block's, gains[2] the next one's. The gain runs linearly
        //    from the smaller of the neighbouring gains at the block's start to its own at the
        //    centre and on to the smaller one at its end: a band switched on inside a block
        //    (codec frames are not aligned with the blocks) is attenuated from its start.
        let b = self.block;
        while self.gains.len() >= 3 && self.low.len() >= b * ch {
            let g1 = self.gains[1];
            let start = self.gains[0].min(g1);
            let end = self.gains[2].min(g1);
            for i in 0..b {
                let pos = i as f64 / b as f64;
                let g = if pos < 0.5 { start + (g1 - start) * 2.0 * pos } else { g1 + (end - g1) * 2.0 * (pos - 0.5) } as f32;
                for _ in 0..ch {
                    let lo = self.low.pop_front().unwrap_or(0.0);
                    let hi = self.high.pop_front().unwrap_or(0.0);
                    self.out.push_back(lo + g * hi);
                }
            }
            self.gains.pop_front();
        }
        let n = frames * ch;
        // The latency covers the blocks still waiting for their look-ahead, so `out` holds
        // at least `n` samples; zeros would only fill in after a programming error.
        let mut result: Vec<f32> = self.out.drain(..n.min(self.out.len())).collect();
        result.resize(n, 0.0);
        result
    }

    /// The last [`Self::latency`] frames of the input, processed (the end of a stream).
    pub fn flush(&mut self) -> Vec<f32> {
        self.process(&vec![0.0; self.latency() * self.channels])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 32_000;

    fn sine(f: f64, amp: f64, i: usize) -> f32 {
        (amp * (2.0 * std::f64::consts::PI * f * i as f64 / f64::from(FS)).sin()) as f32
    }

    fn run(s: &mut HighBandSmoother, x: &[f32], chunk: usize) -> Vec<f32> {
        x.chunks(chunk * s.channels()).flat_map(|c| s.process(c)).collect()
    }

    /// Block RMS in dB.
    fn level(x: &[f32]) -> f64 {
        10.0 * (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64 + 1e-15).log10()
    }

    #[test]
    fn steady_audio_is_only_delayed() {
        let mut s = HighBandSmoother::new(FS, 1, 6000.0).unwrap();
        let lat = s.latency();
        assert_eq!(lat, 110 + 6 * 512, "221 taps, 512-frame blocks");
        let x: Vec<f32> = (0..2 * FS as usize).map(|i| sine(1000.0, 0.3, i) + sine(8000.0, 0.1, i)).collect();
        let y = run(&mut s, &x, 2048);
        assert_eq!(y.len(), x.len());
        assert!(y[..lat].iter().all(|v| *v == 0.0), "zeros during the delay");
        // After the start (the band fades in from the zeros before the input), the output is
        // the delayed input.
        let err = (FS as usize / 2..x.len()).map(|i| (y[i] - x[i - lat]).abs()).fold(0.0f32, f32::max);
        assert!(err < 2e-3, "max deviation {err}");
    }

    #[test]
    fn switching_band_changes_slowly_and_is_never_raised() {
        let mut s = HighBandSmoother::new(FS, 1, 6000.0).unwrap();
        let lat = s.latency();
        let b = 512;
        // A steady 1 kHz tone, and an 8 kHz tone switching between 0.1 and 0.001 (40 dB)
        // every 4 blocks, as a gated SBR band.
        let high = |i: usize| if (i / (4 * b)).is_multiple_of(2) { sine(8000.0, 0.1, i) } else { sine(8000.0, 0.001, i) };
        let x: Vec<f32> = (0..3 * FS as usize).map(|i| sine(1000.0, 0.3, i) + high(i)).collect();
        let y = run(&mut s, &x, 1000);
        // Output high band = output − the delayed 1 kHz tone (which passes unchanged).
        let hb: Vec<f32> = (lat..y.len()).map(|i| y[i] - sine(1000.0, 0.3, i - lat)).collect();
        let inp: Vec<f32> = (0..hb.len()).map(high).collect();
        let lv: Vec<f64> = hb.chunks_exact(b).map(level).collect();
        let li: Vec<f64> = inp.chunks_exact(b).map(level).collect();
        // The input jumps by 40 dB between blocks. The slew limit holds on the smoother's own
        // block grid; these blocks are offset from it by the filter delay (110 frames), as
        // codec frames are in general, so a measured step can be a few dB more.
        for k in 8..lv.len() - 1 {
            let step = (lv[k + 1] - lv[k]).abs();
            assert!(step <= 10.0, "block {k}: {:.1} -> {:.1} dB", lv[k], lv[k + 1]);
            // Never raised — checked away from the switches: an abrupt switch has energy below
            // the crossover too, which the low-pass spreads into the neighbouring blocks.
            if k % 4 == 1 || k % 4 == 2 {
                assert!(lv[k] <= li[k] + 0.5, "block {k} raised: {:.1} dB over {:.1} dB", lv[k], li[k]);
            }
        }
        let (lo, hi) = lv[8..].iter().fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
        assert!(hi - lo <= 12.0, "output swing {:.1} dB", hi - lo);
    }

    #[test]
    fn stereo_and_chunk_sizes_do_not_matter() {
        // Left: low tone with a gated high tone; right: silence. Same result in any chunking,
        // and nothing leaks from left to right.
        let n = FS as usize;
        let x: Vec<f32> = (0..n)
            .flat_map(|i| [sine(500.0, 0.2, i) + if (i / 2048) % 2 == 0 { sine(9000.0, 0.2, i) } else { 0.0 }, 0.0])
            .collect();
        let mut a = HighBandSmoother::new(FS, 2, 6000.0).unwrap();
        let mut b = a.clone();
        let ya = run(&mut a, &x, 4096);
        let mut yb = Vec::new();
        let mut pos = 0;
        for (k, len) in [1usize, 7, 300, 2048, 5, 999].iter().cycle().enumerate() {
            if pos >= n || k > 10_000 {
                break;
            }
            let end = (pos + len).min(n);
            yb.extend(b.process(&x[2 * pos..2 * end]));
            pos = end;
        }
        assert_eq!(ya.len(), yb.len());
        assert!(ya.iter().zip(&yb).all(|(p, q)| p == q));
        assert!(ya.iter().skip(1).step_by(2).all(|v| *v == 0.0), "right channel stays silent");
        assert!(ya.iter().step_by(2).any(|v| v.abs() > 0.1));
    }

    #[test]
    fn flush_returns_the_tail_and_bad_crossovers_are_refused() {
        let mut s = HighBandSmoother::new(FS, 1, 6000.0).unwrap();
        let x: Vec<f32> = (0..10_000).map(|i| sine(1000.0, 0.3, i)).collect();
        let mut y = s.process(&x);
        y.extend(s.flush());
        let lat = s.latency();
        assert_eq!(y.len(), x.len() + lat);
        // Up to two blocks before the end: there the look-ahead fades the high band (the abrupt
        // end's transient) out before the silence.
        let err = (2000..x.len() - 1024).map(|i| (y[i + lat] - x[i]).abs()).fold(0.0f32, f32::max);
        assert!(err < 2e-3, "{err}");
        assert!(HighBandSmoother::new(FS, 1, 0.45 * f64::from(FS)).is_none());
        assert!(HighBandSmoother::new(FS, 1, 0.0).is_none());
        assert!(HighBandSmoother::new(FS, 0, 6000.0).is_none());
    }
}
