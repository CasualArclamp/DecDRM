//! Encode → DRM framing → decode round trips through the receiver's code path: the
//! decoder is opened from the SDC type-9 bytes the encoder reports, frames are passed as
//! byte vectors plus their CRC byte exactly as they would be cut from an audio super frame.

mod common;

use common::{band_energy_db, channel, db, peak_frequency, rms, sine, tone_amplitude};
use decdrm_codecs::{
    AacProfile, AudioInfo, DrmAudioCoding, FdkDrmEncoder, FdkEncoderConfig, OpusDrmEncoder,
    OpusEncoderConfig, OpusSignalling, PcmFrame, open_decoder,
};

/// Result of pushing a signal through encoder and decoder.
struct Run {
    /// Decoded output, concatenated.
    out: Vec<f32>,
    channels: usize,
    rate: u32,
    frame_bytes: Vec<usize>,
    concealed: usize,
    description: String,
}

/// Encodes `frames` frames produced by `input(frame_index)` and decodes them.
fn aac_run(cfg: FdkEncoderConfig, frames: usize, input: impl Fn(usize) -> Vec<f32>) -> Run {
    let mut enc = FdkDrmEncoder::new(cfg.clone()).expect("encoder");
    let t9 = enc.audio_info().to_type9_bytes();
    let info = AudioInfo::from_type9_bytes(&t9).unwrap();
    assert_eq!(info.drm_audio_coding(), Some(DrmAudioCoding::Aac));
    let mut dec = open_decoder(DrmAudioCoding::Aac, &t9).expect("decoder");
    let mut run = Run {
        out: Vec::new(),
        channels: 0,
        rate: 0,
        frame_bytes: Vec::new(),
        concealed: 0,
        description: String::new(),
    };
    for i in 0..frames {
        let pcm = input(i);
        let Some(frame) = enc.encode(&pcm).expect("encode") else { continue };
        let bytes = frame.to_bytes();
        run.frame_bytes.push(bytes.len());
        let out: PcmFrame = dec.decode(&bytes, Some(frame.crc())).expect("decode");
        if out.concealed {
            run.concealed += 1;
        }
        run.channels = usize::from(out.channels);
        run.rate = out.sample_rate;
        run.out.extend_from_slice(&out.samples);
    }
    run.description = dec.describe();
    run
}

fn mono_frame(f: f64, amp: f64, fs: u32, len: usize) -> impl Fn(usize) -> Vec<f32> {
    move |i| sine(f, amp, fs, i * len, len).into_iter().map(|v| v as f32).collect()
}

fn stereo_frame(f: f64, amp_l: f64, amp_r: f64, fs: u32, len: usize) -> impl Fn(usize) -> Vec<f32> {
    move |i| {
        let l = sine(f, amp_l, fs, i * len, len);
        let r = sine(f, amp_r, fs, i * len, len);
        l.iter().zip(&r).flat_map(|(&a, &b)| [a as f32, b as f32]).collect()
    }
}

/// Checks the tone in channel `ch` of the steady-state part of the output.
fn check_tone(run: &Run, ch: usize, f: f64, amp: f64, tol_db: f64) {
    let x = channel(&run.out, run.channels, ch);
    // Skip one second (encoder + decoder delay, SBR/PS start-up).
    let steady = &x[run.rate as usize..];
    let n = steady.len().min(8192);
    let seg = &steady[steady.len() - n..];
    let pf = peak_frequency(seg, run.rate, 100.0, 4000.0);
    assert!((pf - f).abs() < 3.0, "ch{ch}: peak at {pf} Hz, expected {f} Hz");
    let a = tone_amplitude(seg, run.rate, f);
    let err = db(a / amp);
    assert!(err.abs() < tol_db, "ch{ch}: tone level {a:.4} vs {amp} ({err:+.2} dB)");
}

#[test]
fn aac_mono_12khz() {
    let cfg = FdkEncoderConfig::new(AacProfile::Lc, 12_000, 16_000);
    let run = aac_run(cfg, 60, mono_frame(1000.0, 0.5, 12_000, 960));
    eprintln!("{}; frame bytes {:?}", run.description, &run.frame_bytes[..8]);
    assert_eq!(run.rate, 12_000);
    assert_eq!(run.channels, 1);
    assert_eq!(run.concealed, 0, "no frame may fail its CRC");
    assert_eq!(run.description, "AAC mono, 12 kHz");
    check_tone(&run, 0, 1000.0, 0.5, 1.0);
    // 16 kbit/s at 12.5 frames/s = 160 bytes per frame on average.
    let max = *run.frame_bytes.iter().max().unwrap();
    assert!(max <= 170, "largest frame {max} bytes");
}

#[test]
fn aac_stereo_24khz() {
    let mut cfg = FdkEncoderConfig::new(AacProfile::Lc, 24_000, 48_000);
    cfg.stereo = true;
    let run = aac_run(cfg, 100, stereo_frame(1500.0, 0.5, 0.25, 24_000, 960));
    eprintln!("{}; frame bytes {:?}", run.description, &run.frame_bytes[..8]);
    assert_eq!((run.rate, run.channels), (24_000, 2));
    assert_eq!(run.concealed, 0);
    assert_eq!(run.description, "AAC stereo, 24 kHz");
    check_tone(&run, 0, 1500.0, 0.5, 1.0);
    check_tone(&run, 1, 1500.0, 0.25, 1.0);
}

/// HE-AAC: 12 kHz core, SBR to 24 kHz output. A 1 kHz tone lives in the core band; an
/// 8 kHz tone is above the core's Nyquist frequency (6 kHz), so energy there can only come
/// from correctly received and decoded (DRM-syntax, DRM-CRC-protected) SBR data.
#[test]
fn he_aac_sbr_24khz() {
    let cfg = FdkEncoderConfig::new(AacProfile::HeAac, 12_000, 16_000);
    let input = |i: usize| -> Vec<f32> {
        let a = sine(1000.0, 0.4, 24_000, i * 1920, 1920);
        let b = sine(8000.0, 0.2, 24_000, i * 1920, 1920);
        a.iter().zip(&b).map(|(x, y)| (x + y) as f32).collect()
    };
    let run = aac_run(cfg, 50, input);
    eprintln!("{}; frame bytes {:?}", run.description, &run.frame_bytes[..8]);
    assert_eq!((run.rate, run.channels), (24_000, 1));
    assert_eq!(run.concealed, 0);
    assert_eq!(run.description, "HE-AAC (SBR) mono, 12 kHz core, 24 kHz output");
    check_tone(&run, 0, 1000.0, 0.4, 1.5);
    let x = channel(&run.out, 1, 0);
    let seg = &x[x.len() - 8192..];
    let hf = band_energy_db(seg, 24_000, 6500.0, 11_000.0);
    // Input: 8 kHz tone carries 1/5 of the energy (-7 dB). SBR reproduces the band energy.
    eprintln!("SBR band energy {hf:.1} dB of total");
    assert!(hf > -14.0, "SBR band energy {hf:.1} dB — SBR not decoded?");

    // Control: without the 8 kHz tone the band stays empty.
    let cfg = FdkEncoderConfig::new(AacProfile::HeAac, 12_000, 16_000);
    let quiet = aac_run(cfg, 50, mono_frame(1000.0, 0.4, 24_000, 1920));
    let x = channel(&quiet.out, 1, 0);
    let hf_quiet = band_energy_db(&x[x.len() - 8192..], 24_000, 6500.0, 11_000.0);
    eprintln!("control band energy {hf_quiet:.1} dB");
    assert!(hf_quiet < hf - 20.0);
}

/// HE-AAC with a 24 kHz core (48 kHz output).
#[test]
fn he_aac_sbr_48khz() {
    let cfg = FdkEncoderConfig::new(AacProfile::HeAac, 24_000, 32_000);
    let run = aac_run(cfg, 60, mono_frame(2000.0, 0.5, 48_000, 1920));
    eprintln!("{}; frame bytes {:?}", run.description, &run.frame_bytes[..8]);
    assert_eq!((run.rate, run.channels), (48_000, 1));
    assert_eq!(run.concealed, 0);
    check_tone(&run, 0, 2000.0, 0.5, 1.5);
}

/// HE-AAC v2: mono core + SBR + parametric stereo. The left channel is 12 dB louder than
/// the right; only working PS reproduces that (a mono core alone gives L = R).
#[test]
fn he_aac_v2_parametric_stereo() {
    let cfg = FdkEncoderConfig::new(AacProfile::HeAacV2, 12_000, 18_000);
    let run = aac_run(cfg, 60, stereo_frame(1000.0, 0.5, 0.125, 24_000, 1920));
    eprintln!("{}; frame bytes {:?}", run.description, &run.frame_bytes[..8]);
    assert_eq!((run.rate, run.channels), (24_000, 2));
    assert_eq!(run.concealed, 0);
    assert_eq!(run.description, "HE-AAC v2 (SBR+PS) 12 kHz core, 24 kHz stereo");
    let l = channel(&run.out, 2, 0);
    let r = channel(&run.out, 2, 1);
    let n = 8192;
    let (l, r) = (&l[l.len() - n..], &r[r.len() - n..]);
    let (al, ar) = (tone_amplitude(l, 24_000, 1000.0), tone_amplitude(r, 24_000, 1000.0));
    let ild = db(al / ar);
    eprintln!("PS: L {al:.3} R {ar:.3} -> ILD {ild:.1} dB (input 12 dB)");
    assert!((ild - 12.0).abs() < 4.0, "inter-channel level difference {ild:.1} dB");
    assert!((peak_frequency(l, 24_000, 100.0, 4000.0) - 1000.0).abs() < 3.0);
    // Overall level roughly preserved.
    let lvl = db(rms(l) / (0.5 / 2f64.sqrt()));
    assert!(lvl.abs() < 2.0, "left level {lvl:+.1} dB");
}

/// Dream-compatible Opus: 20 ms packets, CRC byte, decoded as a receiver would (SDC type 9
/// signalling → Opus decoder).
#[test]
fn opus_stereo() {
    let mut enc = OpusDrmEncoder::new(OpusEncoderConfig::new(2, 80)).unwrap();
    let t9 = enc.audio_info(OpusSignalling::Legacy).to_type9_bytes();
    let coding = AudioInfo::from_type9_bytes(&t9).unwrap().drm_audio_coding().unwrap();
    assert_eq!(coding, DrmAudioCoding::Opus);
    let mut dec = open_decoder(coding, &t9).unwrap();
    let input = stereo_frame(1000.0, 0.5, 0.25, 48_000, 960);
    let mut out = Vec::new();
    for i in 0..100 {
        let f = enc.encode(&input(i)).unwrap();
        assert_eq!(f.data.len(), 80, "CBR packet size");
        let pcm = dec.decode(&f.data, Some(f.crc)).unwrap();
        assert!(!pcm.concealed);
        assert_eq!((pcm.sample_rate, pcm.channels, pcm.frames()), (48_000, 2, 960));
        out.extend_from_slice(&pcm.samples);
    }
    let run = Run { out, channels: 2, rate: 48_000, frame_bytes: vec![], concealed: 0, description: dec.describe() };
    eprintln!("{}", run.description);
    assert!(run.description.starts_with("Opus CELT fullband stereo, 20 ms"), "{}", run.description);
    check_tone(&run, 0, 1000.0, 0.5, 1.0);
    check_tone(&run, 1, 1000.0, 0.25, 1.0);
}

/// Mono Opus through the dream-mjf signalling (audio coding field 1).
#[test]
fn opus_mono_coding_field_signalling() {
    let mut enc = OpusDrmEncoder::new(OpusEncoderConfig::new(1, 40)).unwrap();
    let t9 = enc.audio_info(OpusSignalling::CodingField).to_type9_bytes();
    let coding = AudioInfo::from_type9_bytes(&t9).unwrap().drm_audio_coding().unwrap();
    let mut dec = open_decoder(coding, &t9).unwrap();
    let input = mono_frame(700.0, 0.5, 48_000, 960);
    let mut out = Vec::new();
    for i in 0..100 {
        let f = enc.encode(&input(i)).unwrap();
        out.extend_from_slice(&dec.decode(&f.data, Some(f.crc)).unwrap().samples);
    }
    // Mono packets are decoded to two identical channels.
    let run = Run { out, channels: 2, rate: 48_000, frame_bytes: vec![], concealed: 0, description: dec.describe() };
    check_tone(&run, 0, 700.0, 0.5, 1.5);
    check_tone(&run, 1, 700.0, 0.5, 1.5);
}
