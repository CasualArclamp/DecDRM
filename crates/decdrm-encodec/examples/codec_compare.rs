//! Compare DAC (`descript/dac_24khz`) with EnCodec on real audio: encode and decode
//! WAV files at EnCodec's bit rates (1.5–24 kbit/s) with both codecs, write what they
//! decode and time them.
//!
//! ```text
//! cargo run --release -p decdrm-encodec --features encodec --example codec_compare -- OUT_DIR IN.wav...
//! ```
//!
//! Inputs: 24 kHz mono WAV files. Outputs: `OUT_DIR/<name>.<codec>.<kbit/s>.wav`
//! (32-bit float). EnCodec runs as DecDRM runs it, streaming 400 ms per call; DAC in
//! 4 s chunks with context. Weights: EnCodec as usual (`decdrm models download
//! encodec`), DAC from `models/dac_24khz/model.safetensors` (or `$DECDRM_DAC`).
//!
//! With `loss` as the first argument it tests lost frames instead: at 6 and 12 kbit/s,
//! groups of three frames (40 ms) lost at random (5 % and 20 % of them, the same for
//! both codecs) are concealed by interpolating the decoder's latent across the gap, as
//! DecDRM's EnCodec decoder does; outputs `<name>.<codec>-loss<percent>.<kbit/s>.wav`.

use decdrm_encodec::dac::DacModel;
use decdrm_encodec::{EncodecModel, FRAME_SAMPLES, LATENT_DIM, ModelParts, SUPER_FRAME_SAMPLES};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Codebooks of the tiers 1.5, 3, 6, 12 and 24 kbit/s (10 bits × 75 frames/s each).
const TIERS: [usize; 5] = [2, 4, 8, 16, 32];

fn read_wav(path: &Path) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    if spec.sample_rate != 24_000 || spec.channels != 1 {
        return Err(format!("{}: need 24 kHz mono, got {} Hz × {}", path.display(), spec.sample_rate, spec.channels).into());
    }
    Ok(match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u32 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()?
        }
    })
}

fn write_wav(path: &Path, pcm: &[f32]) -> Result<(), Box<dyn std::error::Error>> {
    let spec = hound::WavSpec { channels: 1, sample_rate: 24_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in pcm {
        w.write_sample(s)?;
    }
    w.finalize()?;
    Ok(())
}

/// EnCodec as DecDRM streams it: 400 ms per call.
fn encodec(model: &EncodecModel, pcm: &[f32], codebooks: usize) -> Result<(Vec<f32>, f64, f64), Box<dyn std::error::Error>> {
    let (mut enc, mut dec) = (model.encoder_state()?, model.decoder_state()?);
    let t = Instant::now();
    let codes: Vec<Vec<u16>> =
        pcm.as_chunks::<SUPER_FRAME_SAMPLES>().0.iter().map(|c| model.encode(&mut enc, c, codebooks)).collect::<Result<_, _>>()?;
    let encode = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut out = Vec::with_capacity(pcm.len());
    for c in &codes {
        out.extend(model.decode_codes(&mut dec, c, codebooks, codebooks)?);
    }
    Ok((out, encode, t.elapsed().as_secs_f64()))
}

fn dac(model: &DacModel, pcm: &[f32], codebooks: usize) -> Result<(Vec<f32>, f64, f64), Box<dyn std::error::Error>> {
    let t = Instant::now();
    let codes = model.encode(pcm, codebooks)?;
    let encode = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let out = model.decode(&codes, codebooks, codebooks)?;
    Ok((out, encode, t.elapsed().as_secs_f64()))
}

/// Frames lost: groups of three, each lost with probability `p` (a fixed sequence).
fn losses(frames: usize, p: f64, seed: u64) -> Vec<bool> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut lost = vec![false; frames];
    for g in lost.chunks_mut(3) {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let u = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
        if u < p {
            g.fill(true);
        }
    }
    lost
}

/// Replace the latents of lost frames (`dim` values each) by a linear interpolation
/// between the good neighbours (holding the one at an edge).
fn conceal(latents: &mut [f32], dim: usize, lost: &[bool]) {
    let frames = lost.len();
    let mut f = 0;
    while f < frames {
        if !lost[f] {
            f += 1;
            continue;
        }
        let start = f;
        while f < frames && lost[f] {
            f += 1;
        }
        let (prev, next) = (start.checked_sub(1), (f < frames).then_some(f));
        for g in start..f {
            for i in 0..dim {
                latents[g * dim + i] = match (prev, next) {
                    (Some(p), Some(n)) => {
                        let w = (g - p) as f32 / (n - p) as f32;
                        latents[p * dim + i] * (1.0 - w) + latents[n * dim + i] * w
                    }
                    (Some(p), None) => latents[p * dim + i],
                    (None, Some(n)) => latents[n * dim + i],
                    (None, None) => 0.0,
                };
            }
        }
    }
}

/// EnCodec with lost frames concealed in the latent domain.
fn encodec_lossy(model: &EncodecModel, pcm: &[f32], codebooks: usize, lost: &[bool]) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let mut enc = model.encoder_state()?;
    let mut codes = Vec::new();
    for c in pcm.as_chunks::<SUPER_FRAME_SAMPLES>().0 {
        codes.extend(model.encode(&mut enc, c, codebooks)?);
    }
    let frames = codes.len() / codebooks;
    let mut latents = vec![0f32; frames * LATENT_DIM];
    for (f, out) in latents.as_chunks_mut::<LATENT_DIM>().0.iter_mut().enumerate() {
        model.latent(&codes[f * codebooks..(f + 1) * codebooks], codebooks, out)?;
    }
    conceal(&mut latents, LATENT_DIM, &lost[..frames]);
    let mut dec = model.decoder_state()?;
    let mut out = Vec::with_capacity(pcm.len());
    for chunk in latents.chunks(30 * LATENT_DIM) {
        out.extend(model.decode_latents(&mut dec, chunk)?);
    }
    Ok(out)
}

/// DAC with lost frames concealed in the latent domain.
fn dac_lossy(model: &DacModel, pcm: &[f32], codebooks: usize, lost: &[bool]) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let codes = model.encode(pcm, codebooks)?;
    let mut latents = model.latents(&codes, codebooks, codebooks)?;
    let dim = decdrm_encodec::dac::LATENT_DIM;
    let frames = latents.len() / dim;
    conceal(&mut latents, dim, &lost[..frames]);
    Ok(model.decode_latents(&latents)?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let loss_test = args.first().is_some_and(|a| a == "loss");
    if loss_test {
        args.remove(0);
    }
    if args.len() < 2 {
        eprintln!("usage: codec_compare OUT_DIR IN.wav...");
        std::process::exit(2);
    }
    let out_dir = PathBuf::from(&args[0]);
    std::fs::create_dir_all(&out_dir)?;
    let t = Instant::now();
    let enc_model = EncodecModel::load(&decdrm_encodec::find_weights()?, ModelParts::Both)?;
    println!("EnCodec loaded in {:.2} s", t.elapsed().as_secs_f64());
    let dac_path = std::env::var_os("DECDRM_DAC")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/dac_24khz/model.safetensors"));
    let t = Instant::now();
    let dac_model = DacModel::load(&dac_path)?;
    println!("DAC loaded in {:.2} s; candle threads: {}", t.elapsed().as_secs_f64(), candle_core::utils::get_num_threads());
    println!("{:<34} {:>8} {:>6} {:>10} {:>10}", "input", "codec", "kbit/s", "enc RTF", "dec RTF");
    for input in &args[1..] {
        let path = Path::new(input);
        let mut pcm = read_wav(path)?;
        // Whole super frames, so both codecs see the same samples.
        pcm.truncate(pcm.len() / SUPER_FRAME_SAMPLES * SUPER_FRAME_SAMPLES);
        let seconds = pcm.len() as f64 / 24_000.0;
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("input");
        if loss_test {
            let frames = pcm.len() / FRAME_SAMPLES;
            for &n in &[8usize, 16] {
                let kbps = n as f64 * 0.75;
                for (pct, seed) in [(5u32, 1u64), (20, 2)] {
                    let lost = losses(frames, f64::from(pct) / 100.0, seed);
                    let share = lost.iter().filter(|&&l| l).count() as f64 / frames as f64;
                    let e = encodec_lossy(&enc_model, &pcm, n, &lost)?;
                    write_wav(&out_dir.join(format!("{name}.encodec-loss{pct}.{kbps}.wav")), &e)?;
                    let d = dac_lossy(&dac_model, &pcm, n, &lost)?;
                    write_wav(&out_dir.join(format!("{name}.dac-loss{pct}.{kbps}.wav")), &d)?;
                    println!("{name:<34} {kbps:>6} kbit/s: {:.1} % of frames lost", 100.0 * share);
                }
            }
            continue;
        }
        for &n in &TIERS {
            let kbps = n as f64 * 10.0 * 24_000.0 / FRAME_SAMPLES as f64 / 1000.0;
            for (codec, run) in [("encodec", 0), ("dac", 1)] {
                let (out, enc_s, dec_s) = if run == 0 { encodec(&enc_model, &pcm, n)? } else { dac(&dac_model, &pcm, n)? };
                write_wav(&out_dir.join(format!("{name}.{codec}.{kbps}.wav")), &out)?;
                println!("{name:<34} {codec:>8} {kbps:>6} {:>10.3} {:>10.3}", enc_s / seconds, dec_s / seconds);
            }
        }
    }
    Ok(())
}
