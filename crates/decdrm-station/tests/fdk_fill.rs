//! Measurement behind `plan::worst_fill`: the worst size of 5 or 10 consecutive DRM AAC
//! frames relative to their budget (encoder bit rate × 400 ms), per FDK profile and
//! encoder bit rate, over two demanding test signals. Re-run after updating FDK-AAC or
//! the DRM re-packing in `decdrm-codecs`, and copy the printed rows into the table:
//!
//! `cargo test -p decdrm-station --test fdk_fill -- --ignored --nocapture` (~30 s)

use decdrm_codecs::{AacProfile, FdkDrmEncoder, FdkEncoderConfig};

fn noisy(frame: usize, len: usize, fs: u32, channels: usize, seed: &mut u32) -> Vec<f32> {
    let mut out = Vec::with_capacity(len * channels);
    for j in 0..len {
        let t = (frame * len + j) as f64 / f64::from(fs);
        let level = if (t * 1.7).fract() < 0.5 { 0.5 } else { 0.05 };
        for ch in 0..channels {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = f64::from(*seed >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0;
            let tone = 0.3 * (2.0 * std::f64::consts::PI * (440.0 + 200.0 * ch as f64) * t).sin();
            out.push((tone + level * noise).clamp(-1.0, 1.0) as f32);
        }
    }
    out
}

fn music(frame: usize, len: usize, fs: u32, channels: usize, seed: &mut u32) -> Vec<f32> {
    let mut out = Vec::with_capacity(len * channels);
    for j in 0..len {
        let t = (frame * len + j) as f64 / f64::from(fs);
        let f0 = 196.0 * (1.0 + 0.01 * (2.0 * std::f64::consts::PI * 5.0 * t).sin());
        let burst = if (t * 2.5).fract() > 0.7 { 0.4 } else { 0.0 };
        for ch in 0..channels {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = f64::from(*seed >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0;
            let v: f64 = (1..12).map(|h| 0.2 / h as f64 * (2.0 * std::f64::consts::PI * f0 * h as f64 * t + (ch * h) as f64).sin()).sum();
            out.push((v + (0.05 + burst) * noise).clamp(-1.0, 1.0) as f32);
        }
    }
    out
}

#[test]
#[ignore = "measurement (~30 s): prints the worst_fill table"]
fn worst_super_frame_fill() {
    for (profile, rate, stereo) in [
        (AacProfile::Lc, 12_000, false),
        (AacProfile::Lc, 12_000, true),
        (AacProfile::Lc, 24_000, false),
        (AacProfile::Lc, 24_000, true),
        (AacProfile::HeAac, 12_000, false),
        (AacProfile::HeAac, 12_000, true),
        (AacProfile::HeAac, 24_000, false),
        (AacProfile::HeAac, 24_000, true),
        (AacProfile::HeAacV2, 12_000, false),
        (AacProfile::HeAacV2, 24_000, false),
    ] {
        let n = if rate == 12_000 { 5 } else { 10 };
        let mut line = format!("{profile:?} {rate} st={stereo}:");
        for br in [4000u32, 6000, 8000, 10_000, 12_000, 14_000, 16_000, 18_000, 20_000, 24_000, 28_000, 32_000, 40_000] {
            let mut worst = 0.0f64;
            for signal in [music as fn(usize, usize, u32, usize, &mut u32) -> Vec<f32>, noisy] {
                let cfg = FdkEncoderConfig { stereo, ..FdkEncoderConfig::new(profile, rate, br) };
                let mut enc = FdkDrmEncoder::new(cfg.clone()).unwrap();
                let mut seed = 99u32;
                let mut sizes = Vec::new();
                for i in 0..400 {
                    let pcm = signal(i, cfg.frame_len(), cfg.input_sample_rate(), cfg.input_channels(), &mut seed);
                    if let Ok(Some(f)) = enc.encode(&pcm) {
                        sizes.push(f.min_len());
                    }
                }
                let budget = f64::from(br) * 960.0 / f64::from(rate) / 8.0;
                // Super frames start at every n-th frame (any phase: take the worst).
                let w = sizes.windows(n).map(|w| w.iter().sum::<usize>()).max().unwrap() as f64 / (n as f64 * budget);
                worst = worst.max(w);
            }
            line.push_str(&format!(" {}k:{worst:.3}", br / 1000));
        }
        println!("{line}");
    }
}
