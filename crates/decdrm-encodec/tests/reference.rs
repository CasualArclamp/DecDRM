//! Cross-check of the streaming model against candle-transformers' EnCodec, an
//! independent (whole-signal) implementation of the same network, with the same
//! weights. Needs `--features reference` and the weights (skipped without them):
//!
//! ```text
//! cargo test -p decdrm-encodec --features reference --test reference -- --nocapture
//! ```

mod common;

use candle_core::{DType, Device, Tensor};
use candle_transformers::models::encodec;
use decdrm_encodec::{EncodecModel, FRAMES_PER_SUPER_FRAME, ModelParts, SUPER_FRAME_SAMPLES, find_weights};

#[test]
fn streaming_model_matches_candle_transformers() {
    let path = match find_weights() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    let dev = Device::Cpu;
    let vb = candle_nn::VarBuilder::from_buffered_safetensors(std::fs::read(&path).unwrap(), DType::F32, &dev).unwrap();
    // candle-transformers' default configuration is the 24 kHz model.
    let reference = encodec::Model::new(&encodec::Config::default(), vb).unwrap();
    let ours = EncodecModel::load(&path, ModelParts::Both).unwrap();

    // 4 s (10 super frames) of the test programme, in one piece for the reference.
    let x = &common::test_signal()[..10 * SUPER_FRAME_SAMPLES];
    let frames = x.len() / 320;
    let ref_codes: Vec<Vec<u32>> =
        reference.encode(&Tensor::from_slice(x, (1, 1, x.len()), &dev).unwrap()).unwrap().squeeze(0).unwrap().to_vec2().unwrap();
    assert_eq!((ref_codes.len(), ref_codes[0].len()), (32, frames));

    // Ours, streamed super frame by super frame.
    let mut st = ours.encoder_state().unwrap();
    let mut codes = Vec::new();
    for chunk in x.chunks(SUPER_FRAME_SAMPLES) {
        codes.extend(ours.encode(&mut st, chunk, 32).unwrap());
    }
    let mut same = [0usize; 32];
    for (f, frame) in codes.as_chunks::<32>().0.iter().enumerate() {
        for (k, &c) in frame.iter().enumerate() {
            same[k] += usize::from(u32::from(c) == ref_codes[k][f]);
        }
    }
    let total: usize = same.iter().sum();
    println!(
        "codes identical: codebook 0 {}/{frames}, all codebooks {total}/{} ({:.2} %)",
        same[0],
        32 * frames,
        100.0 * total as f64 / (32 * frames) as f64
    );
    assert!(same[0] + 2 >= frames, "codebook 0 differs");
    assert!(total as f64 >= 0.98 * (32 * frames) as f64);

    // Decoders on the same codes (ours): candle-transformers does not trim its
    // transposed convolutions, so its output is longer; compare the common part.
    let t: Vec<u32> = (0..32).flat_map(|k| codes.iter().skip(k).step_by(32).map(|&c| u32::from(c))).collect();
    let code_tensor = Tensor::from_vec(t, (1, 32, frames), &dev).unwrap();
    let y_ref: Vec<f32> = reference.decode(&code_tensor).unwrap().flatten_all().unwrap().to_vec1().unwrap();
    let mut st = ours.decoder_state().unwrap();
    let mut y = Vec::new();
    for chunk in codes.chunks(32 * FRAMES_PER_SUPER_FRAME) {
        y.extend(ours.decode_codes(&mut st, chunk, 32, 32).unwrap());
    }
    assert!(y_ref.len() >= y.len());
    let err: f64 = y.iter().zip(&y_ref).map(|(a, b)| f64::from(a - b).powi(2)).sum();
    let sig: f64 = y_ref[..y.len()].iter().map(|&b| f64::from(b).powi(2)).sum();
    let max = y.iter().zip(&y_ref).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    let snr = 10.0 * (sig / err.max(1e-30)).log10();
    println!("decoder: largest difference {max:.2e}, SNR {snr:.1} dB against candle-transformers");
    assert!(snr > 60.0, "decoders differ: SNR {snr:.1} dB");
}
