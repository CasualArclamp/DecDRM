//! Real-time factor of the streaming DAC encoder and decoder (candle, CPU) in 400 ms
//! steps, as a DRM receiver runs it: 20 s of test signal, 16 codebooks (12 kbit/s);
//! then the DRM encoder and decoder (framing included) mono at 12 kbit/s and joint
//! stereo at 12 kbit/s mid + 1.5 kbit/s side.
//!
//! ```text
//! cargo run --release -p decdrm-dac --features dac --example dac_rtf
//! ```
//!
//! Set `RAYON_NUM_THREADS=1` to measure on a single core. Needs the model weights
//! (`decdrm models download dac`).

#[path = "../tests/common/mod.rs"]
mod common;

use decdrm_dac::model::{DacModel, ENCODER_LEAD_IN, ModelParts};
use decdrm_dac::{Bandwidth, DacConfig, DacDecoder, DacDrmEncoder, LATENT_DIM, SUPER_FRAME_SAMPLES};
use std::sync::Arc;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = decdrm_dac::find_weights()?;
    let t = Instant::now();
    let model = DacModel::load(&path, ModelParts::Both)?;
    println!("DAC loaded in {:.2} s; candle threads: {}", t.elapsed().as_secs_f64(), candle_core::utils::get_num_threads());

    let mut x = common::test_signal();
    while x.len() < 20 * 24_000 {
        x.extend(common::speech_like(2.0));
    }
    x.truncate(20 * 24_000);
    let seconds = x.len() as f64 / 24_000.0;
    let codebooks = 16;

    let mut enc = model.encoder_state()?;
    model.encode_latents(&mut enc, &vec![0.0; ENCODER_LEAD_IN])?;
    let t = Instant::now();
    let mut codes = Vec::new();
    for sf in x.as_chunks::<SUPER_FRAME_SAMPLES>().0 {
        let z = model.encode_latents(&mut enc, sf)?;
        codes.push(model.quantize(&z, codebooks)?);
    }
    let encode = t.elapsed().as_secs_f64();

    let mut dec = model.decoder_state()?;
    let (t, mut worst, mut samples) = (Instant::now(), 0.0f64, 0);
    for c in &codes {
        let t1 = Instant::now();
        let z = model.latents(c, codebooks, codebooks)?;
        assert_eq!(z.len(), 30 * LATENT_DIM);
        samples += model.decode_latents_stream(&mut dec, &z)?.len();
        worst = worst.max(t1.elapsed().as_secs_f64());
    }
    let decode = t.elapsed().as_secs_f64();
    println!("{:.1} s of signal in 400 ms steps, {codebooks} codebooks ({} samples decoded)", seconds, samples);
    println!("encode RTF {:.3}, decode RTF {:.3}, worst decode step {:.1} ms", encode / seconds, decode / seconds, 1000.0 * worst);

    let model = Arc::new(model);
    let mid = DacConfig::new(Bandwidth::Kbps12, 3, 0)?;
    for config in [mid, DacConfig::joint_stereo(mid, DacConfig::new(Bandwidth::Kbps1_5, 3, 0)?)] {
        // Stereo: the signal on the left, a copy 10 ms later on the right.
        let pcm: Vec<f32> = if config.is_stereo() {
            x.iter().enumerate().flat_map(|(i, &v)| [v, if i >= 240 { x[i - 240] } else { 0.0 }]).collect()
        } else {
            x.clone()
        };
        let len = config.min_bytes();
        let mut enc = DacDrmEncoder::new(Arc::clone(&model), config)?;
        let t = Instant::now();
        let sfs = pcm.chunks(SUPER_FRAME_SAMPLES * config.channels()).map(|c| enc.super_frame(c, len)).collect::<Result<Vec<_>, _>>()?;
        let encode = t.elapsed().as_secs_f64();
        let mut dec = DacDecoder::new(Arc::clone(&model), config)?;
        let (t, mut worst) = (Instant::now(), 0.0f64);
        for sf in &sfs {
            let t1 = Instant::now();
            dec.decode_super_frame(sf)?;
            worst = worst.max(t1.elapsed().as_secs_f64());
        }
        let decode = t.elapsed().as_secs_f64();
        println!(
            "DRM {}: encode RTF {:.3}, decode RTF {:.3}, worst decode step {:.1} ms",
            config.rates(),
            encode / seconds,
            decode / seconds,
            1000.0 * worst
        );
    }
    Ok(())
}
