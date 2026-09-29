//! Test signals and measurements shared by the EnCodec tests and examples.

#![allow(dead_code)]

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use std::f64::consts::TAU;

/// Sampling rate of everything here.
pub const RATE: f64 = 24_000.0;
/// Seconds of each part of [`test_signal`].
pub const PART_SECONDS: f64 = 2.0;
/// Frequency of the tone part.
pub const TONE_HZ: f64 = 440.0;

/// `seconds` of a sine.
pub fn tone(freq: f64, amp: f64, seconds: f64) -> Vec<f32> {
    let n = (seconds * RATE) as usize;
    (0..n).map(|i| (amp * (TAU * freq * i as f64 / RATE).sin()) as f32).collect()
}

/// A linear chirp from `f0` to `f1` Hz.
pub fn chirp(f0: f64, f1: f64, amp: f64, seconds: f64) -> Vec<f32> {
    let n = (seconds * RATE) as usize;
    let k = (f1 - f0) / seconds;
    (0..n)
        .map(|i| {
            let t = i as f64 / RATE;
            (amp * (TAU * (f0 * t + 0.5 * k * t * t)).sin()) as f32
        })
        .collect()
}

/// Speech-like harmonic bursts: a gliding 110–170 Hz fundamental with harmonics shaped
/// by three formants, in 170 ms "syllables" with 80 ms pauses.
pub fn speech_like(seconds: f64) -> Vec<f32> {
    let n = (seconds * RATE) as usize;
    let formants = [(600.0, 150.0), (1400.0, 250.0), (2600.0, 350.0)];
    let envelope = |f: f64| -> f64 {
        0.05 + formants.iter().map(|&(c, b): &(f64, f64)| (-((f - c) / b).powi(2)).exp()).sum::<f64>()
    };
    let mut phases = vec![0.0f64; 40];
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / RATE;
        let f0 = 140.0 + 30.0 * (TAU * 0.7 * t).sin();
        // Syllable envelope: 250 ms period, 170 ms on with 20 ms raised-cosine ramps.
        let p = t % 0.25;
        let syl = if p < 0.02 {
            0.5 - 0.5 * (std::f64::consts::PI * p / 0.02).cos()
        } else if p < 0.15 {
            1.0
        } else if p < 0.17 {
            0.5 + 0.5 * (std::f64::consts::PI * (p - 0.15) / 0.02).cos()
        } else {
            0.0
        };
        let mut v = 0.0;
        for (k, ph) in phases.iter_mut().enumerate() {
            let f = f0 * (k + 1) as f64;
            *ph = (*ph + TAU * f / RATE) % TAU;
            if f < 5000.0 {
                v += envelope(f) * ph.sin();
            }
        }
        out.push((0.08 * syl * v) as f32);
    }
    out
}

/// The standard test programme: 2 s tone (440 Hz), 2 s chirp (150 Hz → 5 kHz), 2 s
/// speech-like bursts; 6 s at 24 kHz = 15 super frames.
pub fn test_signal() -> Vec<f32> {
    let mut x = tone(TONE_HZ, 0.3, PART_SECONDS);
    x.extend(chirp(150.0, 5000.0, 0.25, PART_SECONDS));
    x.extend(speech_like(PART_SECONDS));
    x
}

/// Samples of part `k` (0 tone, 1 chirp, 2 speech) of [`test_signal`].
pub fn part(x: &[f32], k: usize) -> &[f32] {
    let n = (PART_SECONDS * RATE) as usize;
    &x[k * n..((k + 1) * n).min(x.len())]
}

/// RMS level, dBFS.
pub fn rms_db(x: &[f32]) -> f64 {
    let p = x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len().max(1) as f64;
    10.0 * p.max(1e-20).log10()
}

/// Frequency with the most power between `lo` and `hi` (1 Hz steps, Goertzel).
pub fn dominant_frequency(x: &[f32], lo: f64, hi: f64) -> f64 {
    let power = |f: f64| {
        let w = TAU * f / RATE;
        let (c, mut s1, mut s2) = (2.0 * w.cos(), 0.0, 0.0);
        for &v in x {
            let s0 = f64::from(v) + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    };
    let (mut best, mut best_p) = (lo, f64::MIN);
    let mut f = lo;
    while f <= hi {
        let p = power(f);
        if p > best_p {
            (best, best_p) = (f, p);
        }
        f += 1.0;
    }
    best
}

/// Lag of `y` against `x` (positive: `y` late) with the largest cross-correlation, over
/// the first `n` samples, within ±`max`.
pub fn best_lag(x: &[f32], y: &[f32], n: usize, max: isize) -> isize {
    let n = n.min(x.len()).min(y.len()) as isize;
    let corr = |lag: isize| -> f64 {
        (max..n - max).map(|i| f64::from(x[i as usize]) * f64::from(y[(i + lag) as usize])).sum()
    };
    (-max..=max).max_by(|&a, &b| corr(a).total_cmp(&corr(b))).expect("non-empty range")
}

/// Log-spectral distance (dB) between the spectral envelopes of `reference` and
/// `test`: 40 ms Hann frames (hop 20 ms), energies in 20 bands log-spaced from 100 Hz
/// to 8 kHz, levels floored 50 dB below the frame's strongest reference band, RMS
/// difference over the bands, averaged over the frames where the reference is above
/// −50 dBFS.
pub fn log_spectral_distance(reference: &[f32], test: &[f32]) -> f64 {
    let d = frame_distances(reference, test);
    d.iter().sum::<f64>() / d.len().max(1) as f64
}

/// The `q`-quantile (0..1) of the per-frame distances of [`log_spectral_distance`].
pub fn distance_quantile(reference: &[f32], test: &[f32], q: f64) -> f64 {
    let mut d = frame_distances(reference, test);
    d.sort_by(f64::total_cmp);
    d.get(((d.len() as f64 * q) as usize).min(d.len().saturating_sub(1))).copied().unwrap_or(0.0)
}

/// Per-frame distances of [`log_spectral_distance`].
pub fn frame_distances(reference: &[f32], test: &[f32]) -> Vec<f64> {
    let (frame, hop, nfft) = (960usize, 480usize, 2048usize);
    let n = reference.len().min(test.len());
    let window: Vec<f64> = (0..frame).map(|i| 0.5 - 0.5 * (TAU * i as f64 / frame as f64).cos()).collect();
    let fft = FftPlanner::<f64>::new().plan_fft_forward(nfft);
    let edges: Vec<usize> = (0..=20)
        .map(|b| {
            let f = 100.0 * (80.0f64).powf(b as f64 / 20.0);
            ((f / RATE * nfft as f64).round() as usize).max(1)
        })
        .collect();
    let bands = |x: &[f32]| -> Vec<f64> {
        let mut buf: Vec<Complex<f64>> = (0..nfft)
            .map(|i| Complex::new(if i < frame { f64::from(x[i]) * window[i] } else { 0.0 }, 0.0))
            .collect();
        fft.process(&mut buf);
        edges
            .windows(2)
            .map(|e| {
                let p: f64 = buf[e[0]..e[1].max(e[0] + 1)].iter().map(|c| c.norm_sqr()).sum();
                10.0 * p.max(1e-30).log10()
            })
            .collect()
    };
    let mut out = Vec::new();
    let mut start = 0;
    while start + frame <= n {
        let r = &reference[start..start + frame];
        if rms_db(r) > -50.0 {
            let (br, bt) = (bands(r), bands(&test[start..start + frame]));
            let floor = br.iter().copied().fold(f64::MIN, f64::max) - 50.0;
            let d: f64 = br.iter().zip(&bt).map(|(a, b)| (a.max(floor) - b.max(floor)).powi(2)).sum::<f64>() / br.len() as f64;
            out.push(d.sqrt());
        }
        start += hop;
    }
    out
}

/// Segmental noise-to-reference ratio (dB) of `test` against `reference`: per 20 ms
/// frame 10·log10(Σ(test − ref)² / Σ ref²), clamped to [−40, 10] dB, averaged over the
/// frames where the reference is above −50 dBFS. Silence instead of the reference
/// scores 0 dB, uncorrelated audio of the same level +3 dB.
pub fn segmental_nrr(reference: &[f32], test: &[f32]) -> f64 {
    let n = reference.len().min(test.len());
    let (mut sum, mut count) = (0.0, 0usize);
    for (r, t) in reference[..n].as_chunks::<480>().0.iter().zip(test[..n].as_chunks::<480>().0) {
        if rms_db(r) <= -50.0 {
            continue;
        }
        let e: f64 = r.iter().zip(t).map(|(&a, &b)| (f64::from(b) - f64::from(a)).powi(2)).sum();
        let s: f64 = r.iter().map(|&a| f64::from(a).powi(2)).sum();
        sum += (10.0 * (e.max(1e-30) / s).log10()).clamp(-40.0, 10.0);
        count += 1;
    }
    sum / count.max(1) as f64
}

/// A small deterministic PRNG (xorshift) for error patterns.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Flip bits of the super frames as a bursty channel would leave them after Viterbi
/// decoding: error events start at `event_rate` per bit and flip each bit of a random
/// 2–16-bit span with probability ½ (first and last bit always). Returns the flipped
/// bit positions per super frame.
pub fn inject_bursts(super_frames: &mut [Vec<u8>], event_rate: f64, rng: &mut Rng) -> Vec<Vec<usize>> {
    super_frames
        .iter_mut()
        .map(|sf| {
            let bits = sf.len() * 8;
            let mut flipped = Vec::new();
            let mut pos = 0;
            while pos < bits {
                if rng.uniform() < event_rate {
                    let span = 2 + (rng.next_u64() % 15) as usize;
                    for k in 0..span.min(bits - pos) {
                        if k == 0 || k == span - 1 || rng.next_u64() & 1 == 1 {
                            sf[(pos + k) / 8] ^= 0x80 >> ((pos + k) % 8);
                            flipped.push(pos + k);
                        }
                    }
                    pos += span;
                } else {
                    pos += 1;
                }
            }
            flipped
        })
        .collect()
}
