//! Real-time factor of the EnCodec encoder and decoder (candle, CPU) at every
//! bandwidth, 20 s of test signal in 400 ms super frames:
//!
//! ```text
//! cargo run --release -p decdrm-encodec --features encodec --example encodec_rtf
//! ```
//!
//! Set `RAYON_NUM_THREADS=1` to measure on a single core. Needs the model weights
//! (`decdrm models download encodec`).

#[path = "../tests/common/mod.rs"]
mod common;

use decdrm_encodec::{
    Bandwidth, EncodecConfig, EncodecDecoder, EncodecDrmEncoder, EncodecModel, FrameLayout, ModelParts,
    SUPER_FRAME_SAMPLES,
};
use std::sync::Arc;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = decdrm_encodec::find_weights()?;
    let t = Instant::now();
    let decoder_only = EncodecModel::load(&path, ModelParts::Decoder)?;
    println!("decoder half loaded in {:.2} s", t.elapsed().as_secs_f64());
    drop(decoder_only);
    let t = Instant::now();
    let model = Arc::new(EncodecModel::load(&path, ModelParts::Both)?);
    println!("whole model loaded in {:.2} s", t.elapsed().as_secs_f64());
    println!("candle threads: {}", candle_core::utils::get_num_threads());

    let mut x = common::test_signal();
    while x.len() < 20 * 24_000 {
        x.extend(common::speech_like(2.0));
    }
    x.truncate(20 * 24_000);
    let seconds = x.len() as f64 / 24_000.0;

    println!("{:>12} {:>12} {:>12} {:>22}", "bandwidth", "encode RTF", "decode RTF", "worst decode (400 ms)");
    for bw in Bandwidth::ALL {
        let config = EncodecConfig::new(bw, 3, 0)?;
        let len = FrameLayout::new(config).min_bytes();
        let mut enc = EncodecDrmEncoder::new(Arc::clone(&model), config)?;
        let t = Instant::now();
        let sfs = x.as_chunks::<SUPER_FRAME_SAMPLES>().0.iter().map(|c| enc.super_frame(c, len)).collect::<Result<Vec<_>, _>>()?;
        let encode = t.elapsed().as_secs_f64();
        let mut dec = EncodecDecoder::new(Arc::clone(&model), config)?;
        let (t, mut worst) = (Instant::now(), 0.0f64);
        for sf in &sfs {
            let t1 = Instant::now();
            dec.decode_super_frame(sf)?;
            worst = worst.max(t1.elapsed().as_secs_f64());
        }
        let decode = t.elapsed().as_secs_f64();
        println!(
            "{:>12} {:>12.4} {:>12.4} {:>19.1} ms",
            bw.to_string(),
            encode / seconds,
            decode / seconds,
            1000.0 * worst
        );
    }
    Ok(())
}
