//! Real-time factor of the streaming DAC encoder and decoder (candle, CPU) in 400 ms
//! steps, as a DRM receiver runs it: 20 s of test signal, 16 codebooks (12 kbit/s).
//!
//! ```text
//! cargo run --release -p decdrm-encodec --features encodec --example dac_rtf
//! ```
//!
//! Set `RAYON_NUM_THREADS=1` to measure on a single core. Needs the DAC weights in
//! `models/dac_24khz/model.safetensors` (or `$DECDRM_DAC`).

#[path = "../tests/common/mod.rs"]
mod common;

use decdrm_encodec::dac::{DacModel, ENCODER_LEAD_IN, LATENT_DIM};
use decdrm_encodec::SUPER_FRAME_SAMPLES;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::var_os("DECDRM_DAC")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/dac_24khz/model.safetensors"));
    let t = Instant::now();
    let model = DacModel::load(&path)?;
    println!("DAC loaded in {:.2} s; candle threads: {}", t.elapsed().as_secs_f64(), candle_core::utils::get_num_threads());

    let mut x = common::test_signal();
    while x.len() < 20 * 24_000 {
        x.extend(common::speech_like(2.0));
    }
    x.truncate(20 * 24_000);
    let seconds = x.len() as f64 / 24_000.0;
    let codebooks = 16;

    let mut enc = model.encoder_state();
    model.encode_latents(&mut enc, &vec![0.0; ENCODER_LEAD_IN])?;
    let t = Instant::now();
    let mut codes = Vec::new();
    for sf in x.as_chunks::<SUPER_FRAME_SAMPLES>().0 {
        let z = model.encode_latents(&mut enc, sf)?;
        codes.push(model.quantize(&z, codebooks)?);
    }
    let encode = t.elapsed().as_secs_f64();

    let mut dec = model.decoder_state();
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
    Ok(())
}
