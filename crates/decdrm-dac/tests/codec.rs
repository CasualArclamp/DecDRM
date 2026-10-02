//! The DAC model through the DRM framing: quality, streaming, bandwidth tiers, bit
//! errors and concealment. Needs `--features dac` and the model weights
//! (`decdrm models download dac`); skipped when the weights are not installed. Run with
//! `--release --nocapture` to see the measurements (the network is slow unoptimised).

mod common;

use common::*;
use decdrm_dac::model::{DECODER_LAG, ENCODER_LEAD_IN};
use decdrm_dac::*;
use std::sync::Arc;
use std::time::Instant;

/// The codec's delay through `DacDrmEncoder` and `DacDecoder`: the encoder's lead-in
/// and the decoder's look-ahead.
const DELAY: usize = ENCODER_LEAD_IN + DECODER_LAG;

/// The model (both halves), or `None` (test skipped) without weights.
fn model() -> Option<Arc<DacModel>> {
    match find_weights() {
        Ok(path) => Some(DacModel::load_cached(&path, ModelParts::Both).expect("load the DAC weights")),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// Encode and frame `pcm` (whole super frames) into super frames of `len` bytes.
fn transmit(model: &Arc<DacModel>, config: DacConfig, pcm: &[f32], len: usize) -> Vec<Vec<u8>> {
    let mut enc = DacDrmEncoder::new(Arc::clone(model), config).unwrap();
    pcm.as_chunks::<SUPER_FRAME_SAMPLES>().0.iter().map(|c| enc.super_frame(c, len).unwrap()).collect()
}

/// Decode super frames.
fn receive(
    model: &Arc<DacModel>,
    config: DacConfig,
    super_frames: &[Vec<u8>],
    policy: CrcPolicy,
    concealment: Concealment,
) -> (Vec<f32>, DecoderStats) {
    let mut dec = DacDecoder::new(Arc::clone(model), config).unwrap();
    dec.set_crc_policy(policy);
    dec.set_concealment(concealment);
    let mut out = Vec::new();
    for sf in super_frames {
        out.extend(dec.decode_super_frame(sf).unwrap().pcm);
    }
    (out, dec.stats())
}

/// `x` and one super frame of silence after it, so the delayed output covers `x`.
fn with_tail(x: &[f32]) -> Vec<f32> {
    let mut v = x.to_vec();
    v.resize(x.len() + SUPER_FRAME_SAMPLES, 0.0);
    v
}

#[test]
fn round_trip_6kbps_through_framing() {
    let Some(model) = model() else { return };
    let x = test_signal();
    let config = DacConfig::new(Bandwidth::Kbps6, 3, 0).unwrap();
    let len = FrameLayout::new(config).min_bytes() + 7;
    let t0 = Instant::now();
    let sfs = transmit(&model, config, &with_tail(&x), len);
    let encode_s = t0.elapsed().as_secs_f64();
    let t1 = Instant::now();
    let (y, stats) = receive(&model, config, &sfs, CrcPolicy::default(), Concealment::default());
    let decode_s = t1.elapsed().as_secs_f64();
    let audio_s = y.len() as f64 / RATE;
    println!(
        "{:.1} s of audio: encode {encode_s:.2} s (RTF {:.3}), decode {decode_s:.2} s (RTF {:.3}) [test profile]",
        audio_s,
        encode_s / audio_s,
        decode_s / audio_s
    );
    assert_eq!(y.len(), x.len() + SUPER_FRAME_SAMPLES);
    assert_eq!((stats.frames_full, stats.frames_concealed, stats.regions_failed), (480, 0, 0));
    let y = &y[DELAY..DELAY + x.len()];

    // What is left of the delay (from the chirp: a tone's correlation repeats every
    // period): the model's own alignment, a few samples.
    let lag = best_lag(part(&x, 1), part(y, 1), 48_000, 400);
    println!("delay of the decoded audio: {} samples ({DELAY} + {lag})", DELAY as isize + lag);
    assert!(lag.abs() <= 16, "{lag} samples more delay than lead-in and look-ahead");
    for (k, name) in ["tone", "chirp", "speech-like"].iter().enumerate() {
        // Skip the first 200 ms of each part (the change of signal).
        let skip = 4800;
        let (a, b) = (&part(&x, k)[skip..], &part(y, k)[skip..]);
        let lsd = log_spectral_distance(a, b);
        println!("{name}: input {:.1} dBFS, output {:.1} dBFS, log-spectral distance {lsd:.2} dB", rms_db(a), rms_db(b));
        assert!((rms_db(a) - rms_db(b)).abs() < 6.0, "{name}: level changed");
        assert!(lsd < 8.0, "{name}: spectral envelope not preserved ({lsd:.2} dB)");
    }
    let f = dominant_frequency(&part(y, 0)[4800..], 100.0, 2000.0);
    println!("dominant frequency of the tone part: {f} Hz");
    assert!((f - TONE_HZ).abs() <= 2.0, "tone decoded at {f} Hz");
}

/// The DRM encoder and decoder, 400 ms at a time, give what whole-signal runs give.
#[test]
fn streaming_equals_whole_signal() {
    let Some(model) = model() else { return };
    let x = test_signal();
    // The encoder: the whole signal after the lead-in, against the stream.
    let mut padded = vec![0.0; ENCODER_LEAD_IN];
    padded.extend(&x);
    let whole = model.encode(&padded, 8).unwrap();
    let mut enc = DacEncoder::new(Arc::clone(&model), 8).unwrap();
    let mut chunked = Vec::new();
    for c in x.chunks(SUPER_FRAME_SAMPLES) {
        chunked.extend(enc.encode(c).unwrap());
    }
    // The stream's codes start with the lead-in's frames: it lags the input by them.
    assert_eq!(chunked.len(), x.len() / FRAME_SAMPLES * 8);
    let same = chunked.iter().zip(&whole).filter(|(a, b)| a == b).count();
    println!("encoder: {same} of {} codes identical", chunked.len());
    // Float rounding may tip a near-tie between two codebook entries.
    assert!(same as f64 >= 0.995 * chunked.len() as f64);

    // The decoder: 400 ms at a time lags the whole-signal decoding by its look-ahead.
    let latents = model.latents(&chunked, 8, 8).unwrap();
    let a = model.decode_latents(&latents).unwrap();
    let mut s = model.decoder_state().unwrap();
    let mut b = Vec::new();
    for c in latents.chunks(FRAMES_PER_SUPER_FRAME * LATENT_DIM) {
        b.extend(model.decode_latents_stream(&mut s, c).unwrap());
    }
    assert_eq!(b.len(), a.len() - DECODER_LAG);
    let diff = a.iter().zip(&b).map(|(p, q)| (p - q).abs()).fold(0.0f32, f32::max);
    println!("decoder: largest difference {diff:.2e}");
    assert!(diff < 1e-3, "chunked decoding differs by {diff}");
}

/// Every bandwidth decodes from the prefix of one set of 32-codebook codes, and more
/// codebooks do not make the envelope worse.
#[test]
fn bandwidth_tiers() {
    let Some(model) = model() else { return };
    let x = test_signal();
    let codes = model.encode(&x, 32).unwrap();
    let mut lsds = Vec::new();
    for bw in Bandwidth::ALL {
        let y = model.decode(&codes, 32, bw.codebooks()).unwrap();
        let lsd = log_spectral_distance(&x[4800..], &y[4800..]);
        println!("{bw:>10}: log-spectral distance {lsd:.2} dB");
        lsds.push(lsd);
    }
    assert!(lsds.iter().all(|&d| d < 10.0));
    assert!(lsds[4] <= lsds[0], "24 kbit/s should beat 1.5 kbit/s: {lsds:?}");
}

/// Which regions (layer, group, copy) a bit position of the super frame belongs to.
fn region_of(layout: &FrameLayout, bit: usize) -> Option<(usize, usize)> {
    let mut pos = 0;
    let layers = (0..layout.layers()).chain(0..layout.config.repeated_layers);
    for l in layers {
        for g in 0..layout.groups() {
            let unit = layout.region_bits(l) + 8;
            if bit < pos + unit {
                return Some((l, g));
            }
            pos += unit;
        }
    }
    None
}

/// Bursty bit errors: the CRCs catch the corrupted regions (no false alarms, rare
/// misses), fine layers degrade, lost base layers are concealed, and concealment beats
/// decoding the corrupted codes.
#[test]
fn bit_errors_detected_and_concealed() {
    let Some(model) = model() else { return };
    let x = test_signal();
    let config = DacConfig::new(Bandwidth::Kbps6, 3, 1).unwrap();
    let layout = FrameLayout::new(config);
    let len = layout.min_bytes();
    let clean = transmit(&model, config, &x, len);
    let (reference, _) = receive(&model, config, &clean, CrcPolicy::default(), Concealment::default());

    let mut noisy = clean.clone();
    let flipped = inject_bursts(&mut noisy, 1.5e-3, &mut Rng::new(7));
    let (mut hit, mut missed, mut false_alarms) = (0, 0, 0);
    for (sf, bits) in noisy.iter().zip(&flipped) {
        let u = layout.unpack(sf).unwrap();
        // A region is corrupted if bits of every copy of it were hit.
        let mut hits = std::collections::BTreeMap::<(usize, usize), usize>::new();
        let mut copies = std::collections::BTreeSet::new();
        for &b in bits {
            if let Some(r) = region_of(&layout, b) {
                let copy = usize::from(b >= layout.main_bits());
                copies.insert((r, copy));
            }
        }
        for &(r, _) in &copies {
            *hits.entry(r).or_default() += 1;
        }
        for l in 0..layout.layers() {
            for g in 0..layout.groups() {
                let n_copies = if l < config.repeated_layers { 2 } else { 1 };
                let corrupted = hits.get(&(l, g)).is_some_and(|&n| n == n_copies);
                match (corrupted, u.region_ok[l][g]) {
                    (true, true) => missed += 1,
                    (true, false) => hit += 1,
                    (false, false) => false_alarms += 1,
                    (false, true) => {}
                }
            }
        }
    }
    println!("corrupted regions: {hit} detected, {missed} missed by the CRC; {false_alarms} false alarms");
    assert!(hit > 30, "the error pattern hit too few regions ({hit})");
    assert_eq!(false_alarms, 0);
    assert!(missed * 50 <= hit, "CRC-8 should miss about 1 in 256 corrupted regions");

    // Bursts: trusting the enhancement layers beats dropping them (the adaptive policy
    // is printed for comparison; `examples/dac_errors.rs` measures the policies).
    let mut lsd = Vec::new();
    for p in [CrcPolicy::Strict, CrcPolicy::TrustEnhancement, CrcPolicy::Adaptive] {
        let (y, stats) = receive(&model, config, &noisy, p, Concealment::Interpolate);
        lsd.push(log_spectral_distance(&reference, &y));
        println!(
            "bursts, {p:?}: LSD {:.2} dB, NRR {:+.1} dB; frames full {} degraded {} concealed {} unverified {}, regions failed {} repaired {}",
            lsd[lsd.len() - 1],
            segmental_nrr(&reference, &y),
            stats.frames_full,
            stats.frames_degraded,
            stats.frames_concealed,
            stats.frames_unverified,
            stats.regions_failed,
            stats.regions_repaired
        );
        assert!(stats.regions_repaired > 0, "the repeated base layer should repair regions");
        assert!(stats.frames_concealed > 0 || stats.frames_unverified > 0);
    }
    assert!(lsd[1] < lsd[0], "trusting enhancement layers should beat dropping them: {lsd:?}");

    // Garbage super frames: concealment beats decoding the garbage.
    let mut garbage = clean.clone();
    let mut rng = Rng::new(11);
    let mut lost = 0;
    for sf in garbage.iter_mut().skip(1).step_by(4) {
        sf.iter_mut().for_each(|b| *b = rng.next_u64() as u8);
        lost += 1;
    }
    let (concealed, stats) = receive(&model, config, &garbage, CrcPolicy::default(), Concealment::default());
    let (ignored, _) = receive(&model, config, &garbage, CrcPolicy::Ignore, Concealment::default());
    let (c, i) = (segmental_nrr(&reference, &concealed), segmental_nrr(&reference, &ignored));
    println!("{lost} garbage super frames: NRR {c:+.1} dB concealed, {i:+.1} dB decoded as received");
    // (A random region passes CRC-8 with probability 1/256, so a few frames of garbage
    // may slip through.) DAC decodes random codes to a quieter sound than EnCodec did,
    // so silence wins by less than it did then (0.9 dB here; the EnCodec test required
    // 1 dB).
    assert!(stats.frames_concealed >= 27 * lost, "{} frames concealed", stats.frames_concealed);
    assert!(c < i, "concealment should beat decoding garbage");
}

/// The concealment output of a lost super frame fades out; the first good frame after
/// fades back in; nothing blows up. (The decoder's output lags by its look-ahead, so a
/// super frame's concealment shows [`DECODER_LAG`] samples into the next output block.)
#[test]
fn lost_super_frames_fade_out_and_back_in() {
    let Some(model) = model() else { return };
    let x = tone(TONE_HZ, 0.3, 4.0);
    let config = DacConfig::new(Bandwidth::Kbps3, 3, 0).unwrap();
    let sfs = transmit(&model, config, &x, 200);
    let mut dec = DacDecoder::new(Arc::clone(&model), config).unwrap();
    let mut out = Vec::new();
    for (i, sf) in sfs.iter().enumerate() {
        let d = if (4..6).contains(&i) { dec.conceal_super_frame() } else { dec.decode_super_frame(sf) }.unwrap();
        out.push(d.pcm);
    }
    let levels: Vec<f64> = out.iter().map(|p| rms_db(p)).collect();
    println!("output block levels (super frames 4 and 5 lost): {:?}", levels.iter().map(|l| format!("{l:.1}")).collect::<Vec<_>>());
    // The first lost super frame holds 40 ms and fades over 80 ms, then silence.
    let lag = DECODER_LAG;
    assert!(rms_db(&out[4][lag..lag + 960]) > -20.0);
    assert!(rms_db(&out[4][lag + 3000..]) < -80.0 && rms_db(&out[5]) < -80.0);
    assert!(levels[7] > -20.0, "no recovery");
    assert!(out.iter().flatten().all(|v| v.is_finite() && v.abs() < 2.0));
    assert_eq!(dec.stats().frames_concealed, 60);
}
