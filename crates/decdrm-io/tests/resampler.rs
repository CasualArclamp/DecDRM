//! Resampler accuracy tests: frequency, amplitude, time alignment, pass band, stop band,
//! streaming consistency and ratio trimming.

use std::f64::consts::{FRAC_PI_2, PI};

use decdrm_io::{resample, Resampler, ResamplerQuality, To48k};

/// A sine on channel 0 (and a cosine on odd channels, like an I/Q pair).
fn sine(freq: f64, rate: u32, frames: usize, amp: f64, channels: usize) -> Vec<f32> {
    let w = 2.0 * PI * freq / f64::from(rate);
    let mut v = Vec::with_capacity(frames * channels);
    for n in 0..frames {
        let (s, c) = (w * n as f64).sin_cos();
        for ch in 0..channels {
            v.push((amp * if ch % 2 == 1 { c } else { s }) as f32);
        }
    }
    v
}

/// Least-squares fit of `A·sin(ωn + φ)` at `freq` over `x[start..end]`; returns
/// `(A, residual RMS, φ)`.
fn fit_phase(x: &[f32], freq: f64, rate: u32, start: usize, end: usize) -> (f64, f64, f64) {
    let w = 2.0 * PI * freq / f64::from(rate);
    let (mut ss, mut sc, mut cc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (n, &v) in x.iter().enumerate().take(end).skip(start) {
        let (s, c) = (w * n as f64).sin_cos();
        ss += s * s;
        sc += s * c;
        cc += c * c;
        xs += f64::from(v) * s;
        xc += f64::from(v) * c;
    }
    let det = ss * cc - sc * sc;
    let a = (xs * cc - xc * sc) / det;
    let b = (xc * ss - xs * sc) / det;
    let mut err = 0.0;
    for (n, &v) in x.iter().enumerate().take(end).skip(start) {
        let (s, c) = (w * n as f64).sin_cos();
        let e = f64::from(v) - (a * s + b * c);
        err += e * e;
    }
    ((a * a + b * b).sqrt(), (err / (end - start) as f64).sqrt(), b.atan2(a))
}

/// Amplitude and residual RMS of a fit (see [`fit_phase`]).
fn fit(x: &[f32], freq: f64, rate: u32, start: usize, end: usize) -> (f64, f64) {
    let (amp, resid, _) = fit_phase(x, freq, rate, start, end);
    (amp, resid)
}

/// Wraps a phase to (-π, π].
fn wrap(mut p: f64) -> f64 {
    while p > PI {
        p -= 2.0 * PI;
    }
    while p <= -PI {
        p += 2.0 * PI;
    }
    p
}

/// Precise frequency estimate: the phase drift between two windows at either end of
/// `x[start..end]` measures the deviation from the nominal `freq`.
fn phase_freq(x: &[f32], freq: f64, rate: u32, start: usize, end: usize) -> f64 {
    let len = (end - start) / 5;
    let (_, _, p1) = fit_phase(x, freq, rate, start, start + len);
    let (_, _, p2) = fit_phase(x, freq, rate, end - len, end);
    let dt = (end - len - start) as f64 / f64::from(rate);
    freq + wrap(p2 - p1) / (2.0 * PI * dt)
}

/// Coarse frequency from interpolated rising zero crossings.
fn zero_cross_freq(x: &[f32], rate: u32, start: usize, end: usize) -> f64 {
    let mut first = None;
    let mut last = 0.0;
    let mut count = 0usize;
    for n in start + 1..end {
        let (a, b) = (f64::from(x[n - 1]), f64::from(x[n]));
        if a < 0.0 && b >= 0.0 {
            let t = (n - 1) as f64 + a / (a - b);
            if first.is_none() {
                first = Some(t);
            } else {
                count += 1;
            }
            last = t;
        }
    }
    count as f64 / ((last - first.unwrap_or(0.0)) / f64::from(rate))
}

fn channel(x: &[f32], channels: usize, c: usize) -> Vec<f32> {
    x.chunks_exact(channels).map(|f| f[c]).collect()
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

fn check_sine_conversion(in_rate: u32, out_rate: u32, freq: f64, channels: usize) {
    let secs = 2.0;
    let frames = (secs * f64::from(in_rate)) as usize;
    let x = sine(freq, in_rate, frames, 0.5, channels);
    let y = resample(&x, channels, in_rate, out_rate, ResamplerQuality::High).unwrap();

    let expected = (frames as f64 * f64::from(out_rate) / f64::from(in_rate)).round() as usize;
    assert_eq!(y.len(), expected * channels, "output length");
    assert!(y.iter().all(|v| v.is_finite()), "NaN/inf in output");

    let (s0, s1) = (out_rate as usize / 10, expected - out_rate as usize / 10);
    let w = 2.0 * PI * freq / f64::from(out_rate);
    for c in 0..channels {
        let ch = channel(&y, channels, c);
        // Coarse check (catches gross errors), then a precise phase-slope estimate.
        let f0 = zero_cross_freq(&ch, out_rate, s0, s1);
        assert!((f0 - freq).abs() < 0.05, "ch{c}: zero-crossing frequency {f0} Hz");
        let f = phase_freq(&ch, freq, out_rate, s0, s1);
        assert!((f - freq).abs() < 1e-3, "ch{c}: frequency {f} Hz, expected {freq}");
        let (amp, resid, phase) = fit_phase(&ch, freq, out_rate, s0, s1);
        assert!(db(amp / 0.5).abs() < 0.01, "ch{c}: amplitude {amp}");
        assert!(db(resid / amp) < -100.0, "ch{c}: residual {:.1} dB", db(resid / amp));
        // Time alignment: the filter delay is removed, so the output is sin(2πft) (cos on
        // the Q channel) sampled at the output rate. The phase error, expressed as a time
        // offset, must be a tiny fraction of a sample period (at most 1/256 of an input
        // sample for integer ratios, where the sinc-table offset cannot be padded away).
        let expected_phase = if c % 2 == 1 { FRAC_PI_2 } else { 0.0 };
        let lead_out = wrap(phase - expected_phase) / w;
        let lead_in = lead_out * f64::from(in_rate) / f64::from(out_rate);
        assert!(
            lead_out.abs() < 0.02 && lead_in.abs() < 0.005,
            "ch{c}: output offset by {lead_out:+.4} output samples"
        );
    }
}

#[test]
fn sine_44k1_to_48k() {
    check_sine_conversion(44_100, 48_000, 1000.0, 1);
    check_sine_conversion(44_100, 48_000, 12_345.0, 1);
}

#[test]
fn sine_24k_to_48k_iq() {
    // A complex (I/Q) tone at 24 kHz, as in the I/Q test recordings.
    check_sine_conversion(24_000, 48_000, 3000.0, 2);
    check_sine_conversion(24_000, 48_000, 9_876.5, 2);
}

#[test]
fn sine_other_ratios() {
    check_sine_conversion(96_000, 48_000, 15_000.0, 1);
    check_sine_conversion(48_000, 44_100, 7_000.0, 1);
    check_sine_conversion(12_000, 48_000, 2_500.0, 1);
}

/// Pass band: flat to 0.45·fs of the lower rate for the High preset.
#[test]
fn passband_flat_to_045_fs() {
    for (in_rate, out_rate) in [(44_100, 48_000), (24_000, 48_000), (96_000, 48_000), (48_000, 44_100)] {
        let fs_min = in_rate.min(out_rate);
        for rel in [0.05, 0.25, 0.40, 0.45] {
            let freq = rel * f64::from(fs_min);
            let frames = in_rate as usize; // 1 s
            let x = sine(freq, in_rate, frames, 0.5, 1);
            let y = resample(&x, 1, in_rate, out_rate, ResamplerQuality::High).unwrap();
            let (s0, s1) = (out_rate as usize / 10, y.len() - out_rate as usize / 10);
            let (amp, _) = fit(&y, freq, out_rate, s0, s1);
            let gain = db(amp / 0.5);
            assert!(gain.abs() < 0.02, "{in_rate}->{out_rate} at {freq:.0} Hz: {gain:.4} dB");
        }
    }
}

/// Stop band: content above the output Nyquist must not alias into the pass band.
#[test]
fn stopband_rejects_aliases() {
    let (in_rate, out_rate) = (96_000u32, 48_000u32);
    for freq in [24_600.0, 26_000.0, 30_000.0, 40_000.0] {
        let x = sine(freq, in_rate, in_rate as usize, 0.5, 1);
        let y = resample(&x, 1, in_rate, out_rate, ResamplerQuality::High).unwrap();
        let (s0, s1) = (out_rate as usize / 10, y.len() - out_rate as usize / 10);
        let rms = (y[s0..s1].iter().map(|&v| f64::from(v).powi(2)).sum::<f64>()
            / (s1 - s0) as f64)
            .sqrt();
        let level = db(rms * std::f64::consts::SQRT_2 / 0.5);
        assert!(level < -100.0, "{freq} Hz leaks at {level:.1} dB");
    }
    // Upsampling: images of an 11 kHz tone at 24 kHz (13 kHz, 35 kHz, ...) are removed.
    let x = sine(11_000.0, 24_000, 24_000, 0.5, 1);
    let y = resample(&x, 1, 24_000, 48_000, ResamplerQuality::High).unwrap();
    let (s0, s1) = (4800, y.len() - 4800);
    let (amp, resid) = fit(&y, 11_000.0, 48_000, s0, s1);
    assert!(db(resid / amp) < -100.0, "image level {:.1} dB", db(resid / amp));
}

#[test]
fn chunked_equals_one_shot() {
    let (in_rate, out_rate, ch) = (44_100, 48_000, 2);
    let x = sine(997.0, in_rate, 30_000, 0.7, ch);
    let one_shot = resample(&x, ch, in_rate, out_rate, ResamplerQuality::High).unwrap();

    let mut rs = Resampler::new(in_rate, out_rate, ch, ResamplerQuality::High).unwrap();
    let mut chunked = Vec::new();
    // Irregular block sizes, including empty blocks and blocks that split frames.
    let sizes = [1usize, 7, 0, 1024, 333, 4096, 2, 1, 2047, 5000];
    let mut pos = 0;
    let mut i = 0;
    while pos < x.len() {
        let n = sizes[i % sizes.len()].min(x.len() - pos);
        chunked.extend(rs.process(&x[pos..pos + n]));
        pos += n;
        i += 1;
    }
    chunked.extend(rs.flush());
    assert_eq!(chunked.len(), one_shot.len());
    let max_diff = chunked
        .iter()
        .zip(&one_shot)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(max_diff < 1e-6, "max difference {max_diff}");

    // After a flush the converter starts a fresh stream with identical results.
    let mut again = rs.process(&x);
    again.extend(rs.flush());
    assert_eq!(again, one_shot);
}

#[test]
fn ratio_adjustment_changes_output_count() {
    let in_rate = 48_000;
    let frames = 480_000; // 10 s
    let x = vec![0.1f32; frames];
    for ppm in [0.0, 1000.0, -1000.0, 5000.0] {
        let mut rs = Resampler::new(in_rate, in_rate, 1, ResamplerQuality::Fast).unwrap();
        rs.set_ratio_adjust_ppm(ppm).unwrap();
        assert!((rs.ratio_adjust_ppm() - ppm).abs() < 1e-6);
        let mut n = rs.process(&x).len();
        n += rs.flush().len();
        let expected = frames as f64 * (1.0 + ppm * 1e-6);
        assert!((n as f64 - expected).abs() <= 2.0, "{ppm} ppm: {n} frames, expected {expected}");
    }
    // Out-of-range and non-finite adjustments are rejected, not applied.
    let mut rs = Resampler::new(48_000, 44_100, 1, ResamplerQuality::Fast).unwrap();
    assert!(rs.set_ratio_adjust(1.5).is_err());
    assert!(rs.set_ratio_adjust(f64::NAN).is_err());
    assert_eq!(rs.ratio_adjust(), 1.0);
}

#[test]
fn ratio_change_mid_stream_is_smooth() {
    // A tone keeps its shape when the ratio is trimmed while running (ramped change).
    let mut rs = Resampler::new(48_000, 48_000, 1, ResamplerQuality::Balanced).unwrap();
    let x = sine(1000.0, 48_000, 48_000, 0.5, 1);
    let mut y = rs.process(&x[..24_000]);
    rs.set_ratio_adjust_ppm(1000.0).unwrap();
    y.extend(rs.process(&x[24_000..]));
    // The largest sample-to-sample step of a 0.5-amplitude 1 kHz sine is 0.5·2π/48 ≈ 0.065.
    let max_step = y.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
    assert!(max_step < 0.0660, "discontinuity: step {max_step}");
}

#[test]
fn invalid_arguments_are_errors() {
    assert!(Resampler::new(0, 48_000, 1, ResamplerQuality::High).is_err());
    assert!(Resampler::new(48_000, 0, 1, ResamplerQuality::High).is_err());
    assert!(Resampler::new(48_000, 44_100, 0, ResamplerQuality::High).is_err());
    assert!(resample(&[0.0; 3], 2, 48_000, 44_100, ResamplerQuality::High).is_err());
    // Empty input is fine.
    assert!(resample(&[], 1, 48_000, 44_100, ResamplerQuality::High).unwrap().is_empty());
    let mut rs = Resampler::new(48_000, 44_100, 1, ResamplerQuality::High).unwrap();
    assert!(rs.flush().is_empty());
}

#[test]
fn to_48k_passthrough_and_conversion() {
    let mut p = To48k::new(48_000, 2).unwrap();
    assert!(p.is_passthrough());
    assert_eq!(p.process(&[1.0, 2.0]), vec![1.0, 2.0]);
    assert!(p.flush().is_empty());
    assert!(p.resampler_mut().is_none());

    let mut c = To48k::new(24_000, 1).unwrap();
    assert!(!c.is_passthrough());
    let mut y = c.process(&vec![0.25; 24_000]);
    y.extend(c.flush());
    assert_eq!(y.len(), 48_000);
    assert!((y[24_000] - 0.25).abs() < 1e-4);
}

/// Prints the measured response of every preset (run with `--ignored --nocapture`).
#[test]
#[ignore]
fn report_quality() {
    use std::time::Instant;
    for q in [ResamplerQuality::Fast, ResamplerQuality::Balanced, ResamplerQuality::High] {
        println!("== {q:?}");
        for (in_rate, out_rate) in [(44_100u32, 48_000u32), (24_000, 48_000), (96_000, 48_000), (48_000, 48_000)] {
            let fs_min = f64::from(in_rate.min(out_rate));
            let mut line = format!("{in_rate:>6}->{out_rate:<6}");
            for rel in [0.1, 0.3, 0.4, 0.42, 0.44, 0.45, 0.46, 0.47, 0.48] {
                let freq = rel * fs_min;
                let x = sine(freq, in_rate, in_rate as usize, 0.5, 1);
                let y = resample(&x, 1, in_rate, out_rate, q).unwrap();
                let (s0, s1) = (out_rate as usize / 10, y.len() - out_rate as usize / 10);
                let (amp, resid) = fit(&y, freq, out_rate, s0, s1);
                line += &format!(" {rel:.2}:{:+.3}dB/{:.0}", db(amp / 0.5), db(resid / amp));
            }
            println!("{line}");
        }
        for freq in [24_600.0, 26_000.0, 30_000.0] {
            let x = sine(freq, 96_000, 96_000, 0.5, 1);
            let y = resample(&x, 1, 96_000, 48_000, q).unwrap();
            let rms = (y[4800..y.len() - 4800].iter().map(|&v| f64::from(v).powi(2)).sum::<f64>()
                / (y.len() - 9600) as f64)
                .sqrt();
            println!("  96k->48k stop band {freq} Hz: {:.1} dB", db(rms * 2f64.sqrt() / 0.5));
        }
        for ch in [1usize, 2] {
            let x = vec![0.1f32; 48_000 * 10 * ch];
            let t = Instant::now();
            let mut rs = Resampler::new(44_100, 48_000, ch, q).unwrap();
            let _ = rs.process(&x);
            let secs = t.elapsed().as_secs_f64();
            println!("  speed {ch} ch: {:.0}x real time", 10.0 * 48_000.0 / 44_100.0 / secs);
        }
    }
}
