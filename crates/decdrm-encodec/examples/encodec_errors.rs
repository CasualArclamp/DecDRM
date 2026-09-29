//! Bit errors against EnCodec's CRC policies and concealment methods. The decoded audio
//! is compared with the error-free decoding by two measures (see `tests/common`):
//!
//! * log-spectral distance (LSD) of 40 ms frames — how far the spectral envelope
//!   strays; it rates silence as far worse than babble of the right colour;
//! * segmental noise-to-reference ratio (NRR) of 20 ms frames — how much wrong signal
//!   there is; silence scores 0 dB, uncorrelated audio of the right level +3 dB.
//!
//! Error models: residual error bursts as DRM's Viterbi decoder leaves them (2–16 bits
//! each, starting at the given rate per bit), and whole super frames replaced by
//! garbage (e.g. while synchronisation is lost).
//!
//! ```text
//! cargo run --release -p decdrm-encodec --features encodec --example encodec_errors
//! ```

#[path = "../tests/common/mod.rs"]
mod common;

use common::{Rng, inject_bursts, log_spectral_distance, segmental_nrr};
use decdrm_encodec::{
    Bandwidth, Concealment, CrcPolicy, EncodecConfig, EncodecDecoder, EncodecDrmEncoder, EncodecModel, FrameLayout,
    ModelParts, SUPER_FRAME_SAMPLES,
};
use std::sync::Arc;

const STRATEGIES: [(&str, CrcPolicy, Concealment); 5] = [
    ("ignore CRCs", CrcPolicy::Ignore, Concealment::Interpolate),
    ("strict+interp", CrcPolicy::Strict, Concealment::Interpolate),
    ("trust+interp", CrcPolicy::TrustEnhancement, Concealment::Interpolate),
    ("trust+repeat", CrcPolicy::TrustEnhancement, Concealment::Repeat),
    ("trust+mute", CrcPolicy::TrustEnhancement, Concealment::Mute),
];

fn decode(model: &Arc<EncodecModel>, config: EncodecConfig, sfs: &[Vec<u8>], p: CrcPolicy, c: Concealment) -> Vec<f32> {
    let mut dec = EncodecDecoder::new(Arc::clone(model), config).expect("decoder");
    dec.set_crc_policy(p);
    dec.set_concealment(c);
    sfs.iter().flat_map(|sf| dec.decode_super_frame(sf).expect("decode").pcm).collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = EncodecModel::load_default(ModelParts::Both)?;
    let mut x = common::test_signal();
    x.extend(common::speech_like(6.0));
    let seeds = 3u64;

    for (bw, repeated) in [(Bandwidth::Kbps6, 0), (Bandwidth::Kbps6, 1), (Bandwidth::Kbps3, 1)] {
        let config = EncodecConfig::new(bw, 3, repeated)?;
        let len = FrameLayout::new(config).min_bytes();
        let mut enc = EncodecDrmEncoder::new(Arc::clone(&model), config)?;
        let clean: Vec<Vec<u8>> =
            x.as_chunks::<SUPER_FRAME_SAMPLES>().0.iter().map(|c| enc.super_frame(c, len)).collect::<Result<_, _>>()?;
        let reference = decode(&model, config, &clean, CrcPolicy::default(), Concealment::default());
        println!("\n{} ({len} bytes per super frame): LSD / NRR against the error-free decoding, dB", config.describe());
        print!("{:>16}", "errors");
        for (name, ..) in STRATEGIES {
            print!("{name:>16}");
        }
        println!();
        let scenarios: [(&str, f64, f64); 7] = [
            ("bursts 1e-4/bit", 1e-4, 0.0),
            ("bursts 3e-4/bit", 3e-4, 0.0),
            ("bursts 1e-3/bit", 1e-3, 0.0),
            ("bursts 3e-3/bit", 3e-3, 0.0),
            ("bursts 1e-2/bit", 1e-2, 0.0),
            ("10 % garbage SF", 0.0, 0.1),
            ("30 % garbage SF", 0.0, 0.3),
        ];
        for (label, rate, garbage) in scenarios {
            let mut results = vec![(0.0, 0.0); STRATEGIES.len()];
            for seed in 0..seeds {
                let mut rng = Rng::new(seed + 1);
                let mut noisy = clean.clone();
                if rate > 0.0 {
                    inject_bursts(&mut noisy, rate, &mut rng);
                }
                for sf in &mut noisy {
                    if rng.uniform() < garbage {
                        sf.iter_mut().for_each(|b| *b = rng.next_u64() as u8);
                    }
                }
                for (r, &(_, p, c)) in results.iter_mut().zip(&STRATEGIES) {
                    let y = decode(&model, config, &noisy, p, c);
                    r.0 += log_spectral_distance(&reference, &y) / seeds as f64;
                    r.1 += segmental_nrr(&reference, &y) / seeds as f64;
                }
            }
            print!("{label:>16}");
            for (lsd, nrr) in results {
                print!("{:>16}", format!("{lsd:.2} / {nrr:+.1}"));
            }
            println!();
        }
    }
    Ok(())
}
